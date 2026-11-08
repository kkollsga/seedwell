//! `seedwell restart`: the app quits and opens again with the same state (SPEC 9A.10), the way an update's installer
//! quits and reopens it (permission full, in the activity log). The command answers once the app has accepted, then,
//! unless `--no-wait`, waits until the app answers again as a new run and says when that started, within a time
//! limit; an app that does not come back is an error (exit 6) that says whether it never quit or never reopened.

use std::time::{Duration, Instant};

use serde_json::json;
use seedwell_control::client::Client;
use seedwell_control::proto::{Empty, ErrorCode, Request};
use seedwell_control::types::{HelloDto, RestartDto};

use crate::args::{Opts, Usage, parse_duration};
use crate::commands::{CliError, Global, R, ask, connect, show_line};

/// The default time limit. The quit waits up to 2 minutes for the trackers to hear `stopped` (the installer up to
/// 130 s), then the app starts.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);
/// How often the waiting asks whether the app answers again.
const EVERY: Duration = Duration::from_millis(500);
/// One look at the socket gives up after this long: a run that is quitting may accept and never answer.
const LOOK: Duration = Duration::from_secs(5);

/// A `restart` command.
#[derive(Clone, Debug, PartialEq)]
pub struct Restart {
    /// Wait for the new run (`--no-wait`: answer once the restart is accepted).
    pub wait: bool,
    /// How long to wait for it (`--timeout`).
    pub timeout: Duration,
}

const USAGE: &str = "usage: seedwell restart [--no-wait] [--timeout <time>]";

/// Words after `restart`.
pub fn parse(args: Vec<String>) -> Result<Restart, Usage> {
    let mut o = Opts::new(args);
    let no_wait = o.flag(&["--no-wait"]);
    let timeout = o.value(&["--timeout"])?.map(|t| parse_duration(&t)).transpose()?;
    if !o.finish()?.is_empty() || (no_wait && timeout.is_some()) || timeout == Some(0) {
        return Err(Usage(USAGE.into()));
    }
    Ok(Restart { wait: !no_wait, timeout: timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs) })
}

/// Run `restart`.
pub async fn run(r: Restart, g: &Global) -> R {
    let mut c = connect(g).await?;
    let (raw, d): (_, RestartDto) = ask(&mut c, Request::Restart(Empty {})).await?;
    drop(c);
    show_line(g, &raw, || accepted(&d));
    if !r.wait {
        return Ok(());
    }
    let back = wait_back(g, &d, r.timeout, EVERY).await?;
    let v = json!({ "running": true, "pid": back.pid, "started_ms": back.started_ms, "server": back.server });
    show_line(g, &v, || again(&back));
    Ok(())
}

/// What the app said when it accepted.
pub fn accepted(d: &RestartDto) -> String {
    format!("{}.", d.note.trim_end_matches('.'))
}

/// The new run.
pub fn again(h: &HelloDto) -> String {
    format!("Seedwell is running again: process {}, started {}.", h.pid, crate::chat::time_s(h.started_ms / 1000))
}

/// What the last look at the socket found.
#[derive(Clone, Copy, PartialEq)]
enum Seen {
    /// The run that was asked to restart still answers.
    Old,
    /// Nothing answers.
    Nothing,
}

/// Waits until a run other than `old` (another process, or a later start of it) answers at the socket, looking every
/// `every`, for at most `timeout`. Exit 6 when none does: whether the old run never quit or nothing came back.
pub async fn wait_back(g: &Global, old: &RestartDto, timeout: Duration, every: Duration) -> R<HelloDto> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let seen = match tokio::time::timeout(LOOK.min(left.max(every)), Client::connect(&g.socket, &g.client)).await {
            Ok(Ok(c)) if (c.hello.pid, c.hello.started_ms) != (old.pid, old.started_ms) => return Ok(c.hello),
            Ok(Ok(_)) => Seen::Old,
            Ok(Err(_)) | Err(_) => Seen::Nothing,
        };
        if Instant::now() >= deadline {
            return Err(gave_up(old, timeout, seen));
        }
        tokio::time::sleep(every.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

fn gave_up(old: &RestartDto, timeout: Duration, seen: Seen) -> CliError {
    let log = if old.log.is_empty() { String::new() } else { format!("; its log: {}", old.log) };
    let secs = timeout.as_secs();
    let message = match seen {
        Seen::Old => format!(
            "Seedwell (process {}) has not quit within {secs} s: it may still be telling trackers it stopped (up to 2 minutes){log}",
            old.pid
        ),
        Seen::Nothing => format!("Seedwell quit but did not answer again within {secs} s{log}; ask the user to open it"),
    };
    CliError { code: 6, message, api_code: Some(ErrorCode::Timeout), data: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Restart, Usage> {
        parse(s.split_whitespace().map(str::to_owned).collect())
    }

    #[test]
    fn the_words_parse() {
        assert_eq!(p("").unwrap(), Restart { wait: true, timeout: DEFAULT_TIMEOUT });
        assert_eq!(p("--timeout 5m").unwrap(), Restart { wait: true, timeout: Duration::from_secs(300) });
        assert_eq!(p("--timeout 90").unwrap().timeout, Duration::from_secs(90));
        assert_eq!(p("--no-wait").unwrap(), Restart { wait: false, timeout: DEFAULT_TIMEOUT });
        for bad in ["now", "--no-wait --timeout 5m", "--timeout 0", "--timeout soon", "--force"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Notify;

    /// A stand-in for one run of the app at `path`: it answers `hello` as process `pid` started at `started_ms`, and
    /// `restart` as the app accepts one (notifying `asked`). Aborting the task ends the run: its listener and every
    /// connection go with it.
    async fn app_run(path: &Path, pid: u32, started_ms: u64, asked: Arc<Notify>) -> tokio::task::JoinHandle<()> {
        let mut bound = seedwell_control::transport::bind(path).await.unwrap();
        tokio::spawn(async move {
            let mut conns = tokio::task::JoinSet::new();
            while let Ok(conn) = bound.listener.accept().await {
                let asked = asked.clone();
                conns.spawn(async move {
                    let (rd, mut wr) = tokio::io::split(conn);
                    let mut lines = BufReader::new(rd).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let result = match req["method"].as_str() {
                            Some("hello") => json!({"protocol": 1, "server": "seedwell test", "permission": "full", "methods": [], "rate_limit_per_sec": 20, "pid": pid, "started_ms": started_ms}),
                            Some("restart") => {
                                asked.notify_one();
                                json!({"note": "Restarting Seedwell", "log": "/state/logs/update.log", "pid": pid, "started_ms": started_ms})
                            }
                            other => panic!("the fake app does not answer {other:?}"),
                        };
                        let mut out = serde_json::to_vec(&json!({"v": 1, "id": req["id"], "result": result})).unwrap();
                        out.push(b'\n');
                        if wr.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                });
            }
        })
    }

    fn global(socket: PathBuf) -> Global {
        Global { json: false, socket, socket_given: true, client: "claude".into(), client_given: true, launch: false, yes_i_know: false }
    }

    /// The run that was asked to restart answers for a while before it quits (its shutdown takes seconds): the
    /// command must not take it for the new run and return at once, nor give up while nothing answers between the
    /// two runs; it returns once the new run (a later start) answers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_waits_past_the_old_run_and_the_gap_for_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("c.sock");
        let asked = Arc::new(Notify::new());
        let old = app_run(&sock, 41, 1_000, asked.clone()).await;
        let swap = {
            let (sock, asked) = (sock.clone(), asked.clone());
            tokio::spawn(async move {
                asked.notified().await;
                tokio::time::sleep(Duration::from_millis(600)).await;
                old.abort();
                let _ = old.await;
                tokio::time::sleep(Duration::from_millis(600)).await;
                Some(app_run(&sock, 41, 2_000, Arc::new(Notify::new())).await)
            })
        };
        let start = Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(20), run(Restart { wait: true, timeout: Duration::from_secs(15) }, &global(sock.clone()))).await;
        r.expect("the command has a time limit").expect("the new run answered");
        assert!(start.elapsed() >= Duration::from_millis(1_200), "it returned after {:?}, before the new run started", start.elapsed());
        let new = swap.await.unwrap().expect("the new run");
        let c = Client::connect(&sock, "claude").await.unwrap();
        assert_eq!((c.hello.pid, c.hello.started_ms), (41, 2_000));
        new.abort();
    }

    /// An app that never quits (its shutdown hangs) or never comes back: the command gives up at its time limit
    /// with exit 6 and says which, instead of waiting for ever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_gives_up_at_its_time_limit_and_says_whether_the_app_quit() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("c.sock");
        let asked = Arc::new(Notify::new());
        let old = app_run(&sock, 41, 1_000, asked.clone()).await;
        let limit = Duration::from_secs(2);
        let start = Instant::now();
        let e = tokio::time::timeout(Duration::from_secs(30), run(Restart { wait: true, timeout: limit }, &global(sock.clone()))).await.expect("it gives up by itself").unwrap_err();
        assert!(start.elapsed() < limit + LOOK + Duration::from_secs(2), "gave up after {:?}", start.elapsed());
        assert_eq!((e.code, e.api_code), (6, Some(ErrorCode::Timeout)));
        assert!(e.message.contains("(process 41) has not quit within 2 s"), "{}", e.message);
        assert!(e.message.contains("/state/logs/update.log"), "{}", e.message);
        asked.notified().await;

        // Asked again, it quits and nothing comes back.
        let quit = tokio::spawn(async move {
            asked.notified().await;
            old.abort();
        });
        let e = tokio::time::timeout(Duration::from_secs(30), run(Restart { wait: true, timeout: limit }, &global(sock.clone()))).await.expect("it gives up by itself").unwrap_err();
        quit.await.unwrap();
        assert_eq!(e.code, 6);
        assert!(e.message.contains("quit but did not answer again within 2 s"), "{}", e.message);
    }

    /// `--no-wait` answers once the app accepted, without looking for the new run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_wait_answers_when_the_restart_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("c.sock");
        let asked = Arc::new(Notify::new());
        let old = app_run(&sock, 41, 1_000, asked.clone()).await;
        let start = Instant::now();
        run(Restart { wait: false, timeout: DEFAULT_TIMEOUT }, &global(sock)).await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(3));
        tokio::time::timeout(Duration::from_secs(1), asked.notified()).await.expect("the app was asked");
        old.abort();
    }
}
