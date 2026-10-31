//! `seedwell update`: what this Seedwell runs, and installing development builds (SPEC 12A). A build is handed over
//! as its zip (`update install`), or the app follows a folder and installs each newer build put there (`update
//! watch`). Both need "Allow development builds" on (Settings › General › Updates, or `update dev-builds on`, which
//! sets that switch; off while a development build runs goes back to the latest release); the app decides, this only
//! asks.
#![allow(missing_docs)] // variant fields are named after the options they hold

use std::path::PathBuf;

use trs_control::proto::{Empty, Request, UpdateDevBuildsParams, UpdateInstallParams, UpdateWatchParams};
use trs_control::types::UpdateStatusDto;

use crate::args::{Opts, Usage};
use crate::commands::{Global, R, ask, connect, show};

/// An `update` command.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// The running version and what the updater does.
    Status,
    /// Install this build's zip.
    Install { zip: PathBuf, sha256: Option<String> },
    /// Follow this folder, or stop (`None`).
    Watch(Option<PathBuf>),
    /// "Allow development builds" on or off.
    DevBuilds(bool),
}

const USAGE: &str = "usage: seedwell update [status] | install <Seedwell-X.Y.Z.N.zip> [--sha256 HEX] | watch <folder> | watch --off | dev-builds on|off";

/// A path as the app needs it: absolute (the app may run in another folder).
fn absolute(p: &str) -> PathBuf {
    let p = PathBuf::from(p);
    if p.is_absolute() { p } else { std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p) }
}

/// Words after `update`.
pub fn parse(args: Vec<String>) -> Result<Cmd, Usage> {
    let mut o = Opts::new(args);
    let sha256 = o.value(&["--sha256"])?;
    let off = o.flag(&["--off"]);
    let pos = o.finish()?;
    let words: Vec<&str> = pos.iter().map(String::as_str).collect();
    Ok(match (words.as_slice(), sha256, off) {
        ([] | ["status"], None, false) => Cmd::Status,
        (["install", zip], sha256, false) => Cmd::Install { zip: absolute(zip), sha256 },
        (["watch", folder], None, false) => Cmd::Watch(Some(absolute(folder))),
        (["watch"], None, true) => Cmd::Watch(None),
        (["dev-builds", "on"], None, false) => Cmd::DevBuilds(true),
        (["dev-builds", "off"], None, false) => Cmd::DevBuilds(false),
        _ => return Err(Usage(USAGE.into())),
    })
}

/// Run an `update` command.
pub async fn run(cmd: Cmd, g: &Global) -> R {
    let mut c = connect(g).await?;
    let req = match cmd {
        Cmd::Status => Request::UpdateStatus(Empty {}),
        Cmd::Install { zip, sha256 } => Request::UpdateInstall(UpdateInstallParams { path: zip.display().to_string(), sha256 }),
        Cmd::Watch(folder) => Request::UpdateWatch(UpdateWatchParams { folder: folder.map(|f| f.display().to_string()) }),
        Cmd::DevBuilds(on) => Request::UpdateDevBuilds(UpdateDevBuildsParams { on }),
    };
    let (raw, d): (_, UpdateStatusDto) = ask(&mut c, req).await?;
    show(g, &raw, || words(&d));
    Ok(())
}

/// What `update` says.
pub fn words(d: &UpdateStatusDto) -> String {
    let mut lines = Vec::new();
    if !d.note.is_empty() {
        lines.push(format!("{}.", d.note.trim_end_matches('.')));
    }
    let version = if d.development { format!("{} (development build)", d.version) } else { d.version.clone() };
    lines.push(format!("Seedwell {version}: {}", d.status));
    lines.push(match (&d.watching, d.dev_builds) {
        (_, false) => "Development builds: off (Settings › General › Updates › Allow development builds, or seedwell update dev-builds on).".into(),
        (Some(f), true) => format!("Development builds: on, watching {f}."),
        (None, true) => "Development builds: on (seedwell update install <zip>, or watch <folder>).".into(),
    });
    if let Some(k) = &d.kept {
        lines.push(format!("Kept to go back to: {k}."));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Cmd, Usage> {
        parse(s.split_whitespace().map(str::to_owned).collect())
    }

    #[test]
    fn the_words_parse() {
        assert_eq!(p("").unwrap(), Cmd::Status);
        assert_eq!(p("status").unwrap(), Cmd::Status);
        // An absolute path as this system writes one ("/b" has no drive on Windows: it is not absolute there).
        let abs = if cfg!(windows) { r"C:\b\Seedwell-0.1.10.3.zip" } else { "/b/Seedwell-0.1.10.3.zip" };
        assert_eq!(
            p(&format!("install {abs} --sha256 ab")).unwrap(),
            Cmd::Install { zip: abs.into(), sha256: Some("ab".into()) }
        );
        match p("install Seedwell-0.1.10.3.zip").unwrap() {
            Cmd::Install { zip, sha256: None } => assert!(zip.is_absolute() && zip.ends_with("Seedwell-0.1.10.3.zip")),
            other => panic!("{other:?}"),
        }
        let dir = if cfg!(windows) { r"C:\b" } else { "/b" };
        assert_eq!(p(&format!("watch {dir}")).unwrap(), Cmd::Watch(Some(dir.into())));
        assert_eq!(p("watch --off").unwrap(), Cmd::Watch(None));
        assert_eq!(p("dev-builds on").unwrap(), Cmd::DevBuilds(true));
        assert_eq!(p("dev-builds off").unwrap(), Cmd::DevBuilds(false));
        for bad in ["install", "watch", "watch /b --off", "status --sha256 x", "now", "install a b", "dev-builds", "dev-builds maybe", "dev-builds on --off", "dev-builds on off"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn it_says_what_runs_and_whether_builds_are_allowed() {
        let d = UpdateStatusDto { version: "0.1.10.3".into(), development: true, dev_builds: true, status: "watching for newer builds".into(), watching: Some("/b".into()), kept: Some("0.1.10.2".into()), note: String::new() };
        assert_eq!(
            words(&d),
            "Seedwell 0.1.10.3 (development build): watching for newer builds\nDevelopment builds: on, watching /b.\nKept to go back to: 0.1.10.2."
        );
        let off = UpdateStatusDto { version: "0.1.10".into(), status: "up to date".into(), note: "Stopped watching".into(), ..Default::default() };
        assert_eq!(
            words(&off),
            "Stopped watching.\nSeedwell 0.1.10: up to date\nDevelopment builds: off (Settings › General › Updates › Allow development builds, or seedwell update dev-builds on)."
        );
    }
}
