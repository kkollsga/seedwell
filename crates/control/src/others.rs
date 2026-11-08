//! Another Seedwell on this computer: an app or a daemon with a state folder of its own, found in the process list
//! (`seedwell_platform::running`), said in one line for `seedwell status` and the app's Advanced settings. A forgotten one
//! keeps its own torrents, folders and chats running, and holds ports: the user should know. Nothing is stopped.

use std::path::Path;

use seedwell_platform::running::{OtherSeedwell, SeedwellKind};

/// One line about the other Seedwells; `None` when there are none.
pub fn line(others: &[OtherSeedwell]) -> Option<String> {
    let one = |o: &OtherSeedwell| {
        let what = match o.kind {
            SeedwellKind::App => "the app",
            SeedwellKind::Daemon => "a daemon",
        };
        match &o.state_dir {
            Some(d) => format!("{what} (process {}) with the state folder {}", o.pid, seedwell_platform::dirs::shown(d)),
            None => format!("{what} (process {}, its state folder unknown)", o.pid),
        }
    };
    let list: Vec<String> = others.iter().map(one).collect();
    let head = match list.len() {
        0 => return None,
        1 => concat!("Another Seedwell is running on this ", seedwell_platform::computer!(), ": ").to_owned(),
        n => format!(concat!("{} other Seedwells are running on this ", seedwell_platform::computer!(), ": "), n),
    };
    let them = if list.len() == 1 { "It keeps" } else { "Each keeps" };
    Some(format!(
        "{head}{}. {them} its own torrents, folders and chats running and uses ports of its own; quit it if it is forgotten.",
        list.join("; ")
    ))
}

/// [`line`] for this process, whose state folder is `mine`, from the process list now.
pub fn line_now(mine: &Path) -> Option<String> {
    line(&seedwell_platform::running::other_seedwells_now(mine))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn other(pid: u32, kind: SeedwellKind, dir: Option<&str>) -> OtherSeedwell {
        OtherSeedwell { pid, kind, state_dir: dir.map(PathBuf::from), exe: PathBuf::from("seedwell") }
    }

    #[test]
    fn one_line_names_each_other_seedwell_and_its_state_folder() {
        let pc = seedwell_platform::computer!();
        assert_eq!(line(&[]), None);
        assert_eq!(
            line(&[other(4120, SeedwellKind::Daemon, Some("/srv/old"))]).unwrap(),
            format!("Another Seedwell is running on this {pc}: a daemon (process 4120) with the state folder /srv/old. It keeps its own torrents, folders and chats running and uses ports of its own; quit it if it is forgotten.")
        );
        let two = line(&[other(1, SeedwellKind::App, None), other(2, SeedwellKind::Daemon, Some("/srv/b"))]).unwrap();
        assert!(two.starts_with(&format!("2 other Seedwells are running on this {pc}: the app (process 1, its state folder unknown); a daemon (process 2)")), "{two}");
        assert!(two.contains("Each keeps"), "{two}");
    }
}
