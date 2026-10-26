//! `seedwell awake`: keep the Mac awake while an agent works (SPEC 9A.7). The agent decides how long its work needs,
//! takes a lease for that long, renews it if the work runs longer, and gives it back when done. Seedwell holds the Mac
//! from idle sleep while a lease is live; the display still sleeps and the screen still locks.
#![allow(missing_docs)] // variant fields are named after the options they hold

use std::time::Duration;

use trs_control::proto::{AwakeSetParams, Empty, Request};
use trs_control::types::AwakeDto;

use crate::args::{Opts, Usage, parse_duration};
use crate::commands::{Global, R, ask, connect, show};

/// An `awake` command.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// Who keeps the Mac awake.
    Status,
    /// Keep it awake for this long.
    Hold(Duration),
    /// Let it sleep again.
    Off,
}

/// `awake` with the agent's name.
#[derive(Clone, Debug, PartialEq)]
pub struct Awake {
    pub cmd: Cmd,
    pub agent: Option<String>,
}

const USAGE: &str = "usage: seedwell awake --for <time> | off | status   (--as NAME)";

/// Words after `awake`.
pub fn parse(args: Vec<String>) -> Result<Awake, Usage> {
    let mut o = Opts::new(args);
    let agent = o.value(&["--as"])?;
    let span = o.value(&["--for"])?.map(|d| parse_duration(&d)).transpose()?;
    let pos = o.finish()?;
    let words: Vec<&str> = pos.iter().map(String::as_str).collect();
    let cmd = match (words.as_slice(), span) {
        ([], Some(0)) | (["off"], None) => Cmd::Off,
        ([], Some(s)) if s > trs_control::awake::MAX.as_secs() => return Err(Usage("at most a day (--for 24h)".into())),
        ([], Some(s)) => Cmd::Hold(Duration::from_secs(s)),
        ([] | ["status"], None) => Cmd::Status,
        (["on"], None) => return Err(Usage("say how long your work needs: seedwell awake --for 3h".into())),
        _ => return Err(Usage(USAGE.into())),
    };
    Ok(Awake { cmd, agent })
}

/// Run an `awake` command.
pub async fn run(a: Awake, g: &Global) -> R {
    let me = crate::chat::label(g, &a.agent, &None);
    match a.cmd {
        Cmd::Status => {
            let mut c = connect(g).await?;
            let (raw, d): (_, AwakeDto) = ask(&mut c, Request::AwakeStatus(Empty {})).await?;
            show(g, &raw, || words(&d, None));
            Ok(())
        }
        Cmd::Hold(span) => set(g, &me, Some(span)).await,
        Cmd::Off => set(g, &me, None).await,
    }
}

async fn set(g: &Global, me: &str, span: Option<Duration>) -> R {
    let mut c = connect(g).await?;
    let key = format!("agent:{me}");
    let req = Request::AwakeSet(AwakeSetParams { key: key.clone(), who: Some(me.to_owned()), secs: span.map(|s| s.as_secs()) });
    let (raw, d): (_, AwakeDto) = ask(&mut c, req).await?;
    show(g, &raw, || words(&d, Some((&key, span.is_some()))));
    Ok(())
}

/// What `awake` says. `mine`: the lease this call took (`true`) or gave back.
pub fn words(d: &AwakeDto, mine: Option<(&str, bool)>) -> String {
    let until = |ms: u64| crate::chat::time(ms / 1000);
    let mut t = match mine {
        Some((key, true)) => match d.leases.iter().find(|l| l.key == key) {
            Some(l) => format!("Keeping the Mac awake for {} until {}.", l.who, until(l.until_ms)),
            None => "The lease is already over.".into(),
        },
        Some((_, false)) => "Gave the lease back.".into(),
        None => String::new(),
    };
    let others: Vec<String> =
        d.leases.iter().filter(|l| mine.is_none_or(|(k, _)| l.key != k)).map(|l| format!("{} until {}", l.who, until(l.until_ms))).collect();
    let sep = |t: &mut String| {
        if !t.is_empty() {
            t.push(' ');
        }
    };
    if mine.is_none() || !others.is_empty() {
        sep(&mut t);
        t.push_str(&match (mine.is_some(), others.is_empty()) {
            (_, true) => "No agent asked to keep the Mac awake.".to_string(),
            (false, false) => format!("Agents keeping the Mac awake: {}.", others.join(", ")),
            (true, false) => format!("Also: {}.", others.join(", ")),
        });
    }
    if !d.leases.is_empty() {
        sep(&mut t);
        t.push_str(if !d.setting {
            "But Seedwell's setting \"Keep awake while agents work\" is off, so the Mac may sleep."
        } else if !d.host_keeps {
            "But this Seedwell runs without the app, so nothing holds the Mac awake."
        } else {
            "The display still sleeps and the screen still locks."
        });
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use trs_control::types::AwakeLeaseDto;

    fn p(s: &str) -> Result<Awake, Usage> {
        parse(s.split_whitespace().map(str::to_owned).collect())
    }

    #[test]
    fn the_words_parse() {
        assert_eq!(p("").unwrap().cmd, Cmd::Status);
        assert_eq!(p("status").unwrap().cmd, Cmd::Status);
        assert_eq!(p("--for 3h --as kglite").unwrap(), Awake { cmd: Cmd::Hold(Duration::from_secs(3 * 3600)), agent: Some("kglite".into()) });
        assert_eq!(p("off").unwrap().cmd, Cmd::Off);
        assert_eq!(p("--for 0").unwrap().cmd, Cmd::Off);
        // No default: the agent says how long its work needs.
        assert!(p("on").unwrap_err().0.contains("--for"));
        for bad in ["--for 2d", "sleep", "--for 1h off", "hook install"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn it_says_who_keeps_the_mac_awake_and_whether_it_holds() {
        let lease = |key: &str, who: &str| AwakeLeaseDto { key: key.into(), who: who.into(), client: "c".into(), since_ms: 0, until_ms: 3_600_000 };
        let d = AwakeDto { leases: vec![lease("agent:kglite", "kglite"), lease("agent:web", "web")], setting: true, host_keeps: true, kept: true };
        let t = words(&d, Some(("agent:kglite", true)));
        assert!(t.starts_with("Keeping the Mac awake for kglite until ") && t.contains("Also: web until") && t.ends_with("the screen still locks."), "{t}");
        assert!(words(&d, None).starts_with("Agents keeping the Mac awake: kglite until"));
        let off = AwakeDto { setting: false, kept: false, ..d.clone() };
        assert!(words(&off, None).contains("setting \"Keep awake while agents work\" is off"));
        let daemon = AwakeDto { host_keeps: false, kept: false, ..d };
        assert!(words(&daemon, None).contains("runs without the app"));
        assert_eq!(words(&AwakeDto::default(), None), "No agent asked to keep the Mac awake.");
        assert_eq!(words(&AwakeDto::default(), Some(("agent:x", false))), "Gave the lease back.");
    }
}
