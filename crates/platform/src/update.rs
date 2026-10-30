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

/// The command that runs `installer_url` for `version` into `install_dir` (the folder that holds the app) from the
/// releases of `repo`. The script text is constant: the values are arguments or environment variables, never part
/// of it, so spaces and quotes in a path cannot change what runs.
#[cfg(not(windows))]
pub fn installer_command(installer_url: &str, install_dir: &str, version: &str, repo: &str) -> Command {
    const SCRIPT: &str = r#"curl -fsSL "$1" | SEEDWELL_REPO_URL="$4" SEEDWELL_INSTALL_DIR="$2" bash -s -- --version "$3""#;
    Command {
        argv: ["/bin/bash", "-c", SCRIPT, "seedwell-update", installer_url, install_dir, version, repo]
            .map(str::to_owned)
            .to_vec(),
        env: Vec::new(),
    }
}

/// The command that runs `installer_url` for `version` into `install_dir` (the folder that holds the app) from the
/// releases of `repo`. The script text is constant: the values are environment variables, never part of it, so
/// spaces and quotes in a path cannot change what runs. Windows PowerShell 5.1 (on every Windows 10 and 11) needs
/// TLS 1.2 asked for by name.
#[cfg(windows)]
pub fn installer_command(installer_url: &str, install_dir: &str, version: &str, repo: &str) -> Command {
    const SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12; \
        & ([scriptblock]::Create((Invoke-RestMethod -UseBasicParsing $env:SEEDWELL_INSTALLER_URL))) -Version $env:SEEDWELL_UPDATE_VERSION";
    Command {
        argv: ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", SCRIPT]
            .map(str::to_owned)
            .to_vec(),
        env: [
            ("SEEDWELL_INSTALLER_URL", installer_url),
            ("SEEDWELL_UPDATE_VERSION", version),
            ("SEEDWELL_INSTALL_DIR", install_dir),
            ("SEEDWELL_REPO_URL", repo),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_values_are_arguments_or_variables_never_script_text() {
        let tricky = "/Users/a b/\"$(rm -rf ~)\"'";
        let c = installer_command("https://h/i", tricky, "1.2.3", "https://h");
        let script_at = c.argv.iter().position(|a| a.contains("SEEDWELL")).unwrap_or(2);
        assert!(!c.argv[script_at].contains(tricky));
        assert!(c.argv.iter().any(|a| a == tricky) || c.env.iter().any(|(_, v)| v == tricky));
        assert!(release_zip("1.2.3").starts_with("Seedwell-1.2.3") && release_zip("1.2.3").ends_with(".zip"));
    }
}
