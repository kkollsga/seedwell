//! What Settings shows of this computer's agent tools (SPEC 9A): whether the `seedwell` command is installed, on the
//! PATH or in one of the folders Settings links it into, and whether the Claude Code skill is (both `SKILL.md` files
//! under `~/.claude/skills`, which may be a symbolic link or a junction to elsewhere); and the app's version line
//! under General ("Seedwell 0.1.16", a development build with its number), which reads nothing.
//!
//! Looking costs disk reads that can be slow (a network folder on the PATH, a skills folder linked elsewhere), so the
//! host never looks on its UI thread: when a tab that shows these opens, or after it linked or installed something,
//! it awaits `refresh_settings_host_facts` (a thread of its own), and it draws from what was found last
//! (`settings_host_facts`, which reads nothing) in between. Both hosts use these, not checks of their own.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::Core;

/// What the host passes to the settings model in `SettingsLive::host`, as the core found it.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct SettingsHostFacts {
    /// Where `seedwell` is: a folder of the PATH, else a folder Settings links it into; `None`: not installed.
    pub cli: Option<String>,
    /// Both skills are there: `seedwell` and, beside it, `seedwell-agents` (since 0.1.8; one installed earlier has
    /// only the first, and Install brings the second).
    pub skill_installed: bool,
    /// "Seedwell 0.1.16", or "Seedwell 0.1.16.3 (development build)": the version running (General's last line).
    pub version_line: String,
}

/// The command's file name on this system (`seedwell`, `seedwell.exe`).
fn cli_file_name() -> String {
    format!("seedwell{}", std::env::consts::EXE_SUFFIX)
}

/// Where `seedwell` is: the first folder of `path_env` that has it as a program, else the first of `link_folders`.
pub(crate) fn find_cli(path_env: Option<&OsStr>, link_folders: &[PathBuf]) -> Option<PathBuf> {
    let name = cli_file_name();
    let path = path_env.map(|p| std::env::split_paths(p).collect::<Vec<_>>()).unwrap_or_default();
    path.into_iter().filter(|d| d.is_absolute()).chain(link_folders.iter().cloned()).map(|d| d.join(&name)).find(|p| is_program(p))
}

/// A file that runs: a file (through links), with an execute bit where the system has them.
fn is_program(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && trs_platform::fs::unix_mode(&m).is_none_or(|mode| mode & 0o111 != 0))
}

/// Both skills' `SKILL.md` in `~/.claude/skills`, through links and junctions.
pub(crate) fn skill_installed(home: &Path) -> bool {
    let main = PathBuf::from(crate::settings_view::skill_dir(&home.display().to_string()));
    let agents = main.with_file_name("seedwell-agents");
    [main, agents].iter().all(|d| std::fs::metadata(d.join("SKILL.md")).is_ok_and(|m| m.is_file()))
}

/// General's version line for the running version (`X.Y.Z`, `X.Y.Z.N` for a development build).
pub(crate) fn version_line(running: &str) -> String {
    format!("{} {}", crate::menu::APP_NAME, crate::updates::shown(running))
}

/// What this computer has now (it reads the disk).
fn look(home: Option<&Path>, path_env: Option<&OsStr>) -> SettingsHostFacts {
    let links: Vec<PathBuf> = home.map(|h| crate::settings_view::cli_folders(&h.display().to_string()).map(PathBuf::from).to_vec()).unwrap_or_default();
    SettingsHostFacts {
        cli: find_cli(path_env, &links).map(|p| p.display().to_string()),
        skill_installed: home.is_some_and(skill_installed),
        version_line: String::new(),
    }
}

/// What was found last ([`Core::settings_host_facts`]), and the version running.
#[derive(Default)]
pub(crate) struct HostFacts {
    found: std::sync::Mutex<SettingsHostFacts>,
    /// The updater's running version (`X.Y.Z.N` for a development build); `None` before there is an updater.
    running: std::sync::Mutex<Option<String>>,
}

impl HostFacts {
    fn get(&self) -> SettingsHostFacts {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut f = self.found.lock().unwrap_or_else(|e| e.into_inner()).clone();
        f.version_line = version_line(running.as_deref().unwrap_or(env!("CARGO_PKG_VERSION")));
        f
    }

    fn set(&self, f: SettingsHostFacts) {
        *self.found.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    /// The updater says which version runs (a development build's number is known to it alone).
    pub(crate) fn set_running(&self, version: &str) {
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(version.to_owned());
    }
}

#[uniffi::export]
impl Core {
    /// What Settings shows of this computer's agent tools, as found last (nothing found yet: not installed). Reads
    /// nothing: safe on the host's UI thread.
    pub fn settings_host_facts(&self) -> SettingsHostFacts {
        self.host_facts.get()
    }
}

impl Core {
    /// Looks at this computer again (the disk), keeps what it found for [`Core::settings_host_facts`] and returns it.
    /// Exported `async` (`async_calls`), run on a thread of its own.
    pub(crate) fn refresh_settings_host_facts(&self) -> SettingsHostFacts {
        self.host_facts.set(look(trs_platform::dirs::home().as_deref(), std::env::var_os("PATH").as_deref()));
        self.host_facts.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(cli_file_name());
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        p
    }

    /// On the PATH first, then in a folder Settings links it into; a folder of the PATH that is not absolute (a
    /// relative one would depend on where the app was started) is passed over.
    #[test]
    fn the_command_is_found_on_the_path_or_where_settings_links_it() {
        let t = tempfile::tempdir().unwrap();
        let (on_path, linked, relative) = (t.path().join("bin"), t.path().join("home/.local/bin"), PathBuf::from("rel"));
        let links = [t.path().join("usr-local-bin"), linked.clone()];
        let path = std::env::join_paths([relative.as_path(), on_path.as_path()]).unwrap();
        assert_eq!(find_cli(Some(&path), &links), None);
        let l = program(&linked);
        assert_eq!(find_cli(Some(&path), &links), Some(l.clone()));
        assert_eq!(find_cli(None, &links), Some(l));
        let p = program(&on_path);
        assert_eq!(find_cli(Some(&path), &links), Some(p), "the PATH's comes first");
    }

    /// A file without an execute bit is not the command (where the system has them).
    #[cfg(unix)]
    #[test]
    fn a_file_that_does_not_run_is_not_the_command() {
        let t = tempfile::tempdir().unwrap();
        let p = program(t.path());
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        assert_eq!(find_cli(None, &[t.path().to_path_buf()]), None);
        std::fs::create_dir_all(t.path().join("d").join(cli_file_name())).unwrap();
        assert_eq!(find_cli(None, &[t.path().join("d")]), None, "a folder of that name");
    }

    /// Both skills, each with its SKILL.md; the skills folder may be a link to elsewhere (a symbolic link on a Mac, a
    /// junction or a symbolic link on Windows).
    #[test]
    fn the_skill_is_installed_when_both_skills_are_there() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let skill = |dir: &Path, name: &str| {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("SKILL.md"), "---\nname: x\n---\n").unwrap();
        };
        assert!(!skill_installed(&home));
        let skills = home.join(".claude").join("skills");
        skill(&skills, "seedwell");
        assert!(!skill_installed(&home), "the agents skill is missing (installed before 0.1.8)");
        skill(&skills, "seedwell-agents");
        assert!(skill_installed(&home));
        std::fs::remove_file(skills.join("seedwell-agents").join("SKILL.md")).unwrap();
        std::fs::create_dir(skills.join("seedwell-agents").join("SKILL.md")).unwrap();
        assert!(!skill_installed(&home), "a folder is not the file");

        // ~/.claude/skills a link to a folder elsewhere.
        let elsewhere = t.path().join("dotfiles").join("skills");
        skill(&elsewhere, "seedwell");
        skill(&elsewhere, "seedwell-agents");
        let linked = t.path().join("linked");
        std::fs::create_dir_all(linked.join(".claude")).unwrap();
        let link = linked.join(".claude").join("skills");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(&elsewhere, &link).is_err() {
            // No right to make symbolic links (Developer Mode off): a junction, which needs none.
            let made = std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(&link).arg(&elsewhere).status();
            assert!(made.is_ok_and(|s| s.success()), "a junction");
        }
        assert!(skill_installed(&linked));
        assert!(look(Some(&linked), None).skill_installed);
    }

    /// The host's call: nothing until it looks; then what it found, kept. The version line is the build's version
    /// until the updater says which runs (a development build's number).
    #[test]
    fn what_was_found_is_kept_for_the_ui_thread() {
        let facts = HostFacts::default();
        let release = format!("Seedwell {}", env!("CARGO_PKG_VERSION"));
        assert_eq!(facts.get(), SettingsHostFacts { version_line: release.clone(), ..Default::default() });
        let t = tempfile::tempdir().unwrap();
        program(&t.path().join(".local/bin"));
        let found = look(Some(t.path()), None);
        assert_eq!(found.cli.as_deref().map(Path::new), Some(t.path().join(".local/bin").join(cli_file_name()).as_path()));
        facts.set(found.clone());
        assert_eq!(facts.get(), SettingsHostFacts { version_line: release, ..found });
        facts.set_running("0.1.16.3");
        assert_eq!(facts.get().version_line, "Seedwell 0.1.16.3 (development build)");
        facts.set_running("0.1.16");
        assert_eq!(facts.get().version_line, "Seedwell 0.1.16");
    }
}
