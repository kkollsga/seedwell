//! What an app update looks like on this system: the release's installer and zip, this system's entry in
//! `latest.json`, and the command that runs the installer. The rules of when to check and install are the core's
//! (`trs-ffi`'s `updates.rs`); this is only the part that differs per system.

/// This system's installer among the release's assets (it downloads, checks and installs the zip, then reopens
/// the app).
#[cfg(not(windows))]
pub const INSTALLER: &str = "install.sh";
/// This system's installer among the release's assets (it downloads, checks and installs the zip, then reopens
/// the app).
#[cfg(windows)]
pub const INSTALLER: &str = "install.ps1";

/// Where this system's release is described in `latest.json`: `None` is the top-level fields (the Mac's, which
/// every installed Mac copy reads, so they never move), else the object under that key.
#[cfg(not(windows))]
pub const RELEASE_SECTION: Option<&str> = None;
/// Where this system's release is described in `latest.json`: `None` is the top-level fields (the Mac's, which
/// every installed Mac copy reads, so they never move), else the object under that key.
#[cfg(windows)]
pub const RELEASE_SECTION: Option<&str> = Some("windows");

/// The key of the oldest system version the release runs on, in this system's entry.
#[cfg(not(windows))]
pub const MINIMUM_SYSTEM_KEY: &str = "minimum_macos";
/// The key of the oldest system version the release runs on, in this system's entry.
#[cfg(windows)]
pub const MINIMUM_SYSTEM_KEY: &str = "minimum_windows";

/// The release zip for this system, as the release scripts name it.
#[cfg(not(windows))]
pub fn release_zip(version: &str) -> String {
    format!("Seedwell-{version}.zip")
}
/// The release zip for this system, as the release scripts name it.
#[cfg(windows)]
pub fn release_zip(version: &str) -> String {
    format!("Seedwell-{version}-windows-x64.zip")
}

/// A command for the host to start so that it outlives the app it quits, without a window, its output appended to
/// the update log: on the Mac in a session of its own (`POSIX_SPAWN_SETSID`); on Windows with `CREATE_NO_WINDOW` (a
/// console of its own that is never shown), never `DETACHED_PROCESS` (Windows PowerShell 5.1 without a console runs
/// nothing and exits 0), in a working folder outside the install folder (Windows does not rename a process's current
/// folder). `argv[0]` is the program, `env` is added to the app's own environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The program and its arguments.
    pub argv: Vec<String>,
    /// Variables added to the environment.
    pub env: Vec<(String, String)>,
}

/// What every install is given besides the build: the folder that holds the app, the app's state folder (the
/// installer keeps the app it replaces there, as the last one that started fine, and waits for the new one to start
/// fine: SPEC 12A, Recovery) and the arguments to reopen the app with. They reach the installer as environment
/// variables (`SEEDWELL_INSTALL_DIR`, `SEEDWELL_INSTALL_STATE_DIR`, `SEEDWELL_RELAUNCH_ARGS`, one argument a line), so
/// an older release's installer, which knows none of them but the first, still installs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install<'a> {
    /// The folder that holds the app.
    pub install_dir: &'a str,
    /// The app's state folder.
    pub state_dir: &'a str,
    /// The arguments the app is reopened with (`--background`, and an isolated run's own).
    pub relaunch: &'a [String],
}

impl Install<'_> {
    fn env(&self) -> Vec<(String, String)> {
        [("SEEDWELL_INSTALL_DIR", self.install_dir.to_owned()), ("SEEDWELL_INSTALL_STATE_DIR", self.state_dir.to_owned()), ("SEEDWELL_RELAUNCH_ARGS", self.relaunch.join("\n"))]
            .map(|(k, v)| (k.to_owned(), v))
            .to_vec()
    }
}

/// The command that runs the release's `installer_url` for `version` (`None`: the latest) from the releases of
/// `repo`. The script text is constant: the values are arguments or environment variables, never part of it, so
/// spaces and quotes in a path cannot change what runs.
#[cfg(not(windows))]
pub fn installer_command(installer_url: &str, version: Option<&str>, repo: &str, i: &Install) -> Command {
    const SCRIPT: &str = r#"a=(); [ -n "$2" ] && a=(--version "$2"); curl -fsSL "$1" | SEEDWELL_REPO_URL="$3" bash -s -- "${a[@]}""#;
    let argv = ["/bin/bash", "-c", SCRIPT, "seedwell-update", installer_url, version.unwrap_or(""), repo].map(str::to_owned).to_vec();
    Command { argv, env: i.env() }
}

/// The command that runs the installer `script` (a local file) on `zip`, checked against `sha256`: a build handed
/// over with `seedwell update install`.
#[cfg(not(windows))]
pub fn local_installer_command(script: &str, zip: &str, sha256: &str, i: &Install) -> Command {
    let argv = ["/bin/bash", script, "--zip", zip, "--sha256", sha256].map(str::to_owned).to_vec();
    Command { argv, env: i.env() }
}

/// The command that puts back the app kept in the state folder, with the installer kept next to it (`script`).
#[cfg(not(windows))]
pub fn rollback_command(script: &str, i: &Install) -> Command {
    Command { argv: ["/bin/bash", script, "--rollback"].map(str::to_owned).to_vec(), env: i.env() }
}

/// The command that runs the release's `installer_url` for `version` (`None`: the latest) from the releases of
/// `repo`. The script text is constant: the values are environment variables, never part of it, so spaces and quotes
/// in a path cannot change what runs. Windows PowerShell 5.1 (on every Windows 10 and 11) needs TLS 1.2 asked for by
/// name.
#[cfg(windows)]
pub fn installer_command(installer_url: &str, version: Option<&str>, repo: &str, i: &Install) -> Command {
    const SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12; \
        $a = @{}; if ($env:SEEDWELL_UPDATE_VERSION) { $a.Version = $env:SEEDWELL_UPDATE_VERSION }; \
        & ([scriptblock]::Create((Invoke-RestMethod -UseBasicParsing $env:SEEDWELL_INSTALLER_URL))) @a";
    let mut c = powershell(&["-Command", SCRIPT], i);
    c.env.extend(
        [("SEEDWELL_INSTALLER_URL", installer_url), ("SEEDWELL_UPDATE_VERSION", version.unwrap_or("")), ("SEEDWELL_REPO_URL", repo)]
            .map(|(k, v)| (k.to_owned(), v.to_owned())),
    );
    c
}

/// The command that runs the installer `script` (a local file) on `zip`, checked against `sha256`: a build handed
/// over with `seedwell update install`.
#[cfg(windows)]
pub fn local_installer_command(script: &str, zip: &str, sha256: &str, i: &Install) -> Command {
    powershell(&["-File", script, "-Zip", zip, "-Sha256", sha256], i)
}

/// The command that puts back the app kept in the state folder, with the installer kept next to it (`script`).
#[cfg(windows)]
pub fn rollback_command(script: &str, i: &Install) -> Command {
    powershell(&["-File", script, "-Rollback"], i)
}

#[cfg(windows)]
fn powershell(args: &[&str], i: &Install) -> Command {
    let mut argv: Vec<String> = ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"].map(str::to_owned).to_vec();
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    Command { argv, env: i.env() }
}

/// Starts `c` so that it outlives the app it is about to quit: no window, standard input empty, its output appended to
/// `log` (made if missing); on the Mac in a session of its own, on Windows with `CREATE_NO_WINDOW` in a process group
/// of its own and in the temporary folder (see [`Command`]). It is not waited for.
pub fn spawn(c: &Command, log: &std::path::Path) -> std::io::Result<()> {
    use std::process::Stdio;
    let (program, args) = c.argv.split_first().ok_or_else(|| std::io::Error::other("no program"))?;
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let mut p = std::process::Command::new(program);
    p.args(args).envs(c.env.iter().map(|(k, v)| (k, v))).stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: setsid is async-signal-safe and touches nothing of the parent; a failure (already a group leader,
        // which a fresh child is not) leaves the child in the app's session, where it still runs.
        unsafe {
            p.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
        p.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP).current_dir(std::env::temp_dir());
    }
    let mut child = p.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_values_are_arguments_or_variables_never_script_text() {
        let tricky = "/Users/a b/\"$(rm -rf ~)\"'";
        let relaunch = vec!["--state-dir".to_owned(), tricky.to_owned()];
        let i = Install { install_dir: tricky, state_dir: tricky, relaunch: &relaunch };
        let c = installer_command("https://h/i", Some("1.2.3"), "https://h", &i);
        let script_at = c.argv.iter().position(|a| a.contains("SEEDWELL") || a.contains("curl")).unwrap_or(2);
        assert!(!c.argv[script_at].contains(tricky));
        for c in [c, local_installer_command(tricky, tricky, "ab", &i), rollback_command(tricky, &i)] {
            // A script file is an argument, never script text; the places come in the environment.
            assert!(c.argv.iter().all(|a| a == tricky || !a.contains("rm -rf")), "{c:?}");
            let var = |k: &str| c.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            assert_eq!(var("SEEDWELL_INSTALL_DIR").as_deref(), Some(tricky));
            assert_eq!(var("SEEDWELL_INSTALL_STATE_DIR").as_deref(), Some(tricky));
            assert_eq!(var("SEEDWELL_RELAUNCH_ARGS"), Some(format!("--state-dir\n{tricky}")));
        }
        assert!(release_zip("1.2.3").starts_with("Seedwell-1.2.3") && release_zip("1.2.3").ends_with(".zip"));
    }

    #[cfg(unix)]
    #[test]
    fn a_spawned_installer_writes_its_log_and_outlives_nothing_it_needs() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("logs").join("update.log");
        let c = Command { argv: ["/bin/sh", "-c", "echo hello \"$X\"; [ -t 0 ] && echo tty || echo no-tty"].map(str::to_owned).to_vec(), env: vec![("X".into(), "there".into())] };
        spawn(&c, &log).unwrap();
        for _ in 0..100 {
            if std::fs::read_to_string(&log).is_ok_and(|t| t.contains("no-tty")) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "hello there\nno-tty\n");
    }
}
