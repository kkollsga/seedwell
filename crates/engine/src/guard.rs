//! A safety net for development: when the engine runs under `cargo test` (or a child process of
//! one, such as a daemon started by a test), it refuses to use any folder inside the real home
//! directory. A default `config.toml` points at `~/Downloads`, `~/Media` and the Trash; a test
//! that forgot to set its own folders would otherwise move real files.
//!
//! Active when the running executable is a cargo test binary (it lives in a `deps` folder), or the
//! environment has `TRS_FORBID_REAL_HOME`. A test that starts a *child* process (a daemon built
//! from another crate) must pass `TRS_FORBID_REAL_HOME=1` to it, because the child is not a test
//! binary itself. Tests either point every folder into a temp directory or set `HOME` to one.
//! `TRS_ALLOW_REAL_HOME=1` switches the guard off.

use std::path::{Path, PathBuf};

use crate::config::{AfterAdd, EngineConfig};
use crate::organize::expand_tilde;

fn is_test_binary() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| d.file_name()).map(|n| n == "deps"))
        .unwrap_or(false)
}

fn active() -> bool {
    (cfg!(test) || is_test_binary() || std::env::var_os("TRS_FORBID_REAL_HOME").is_some())
        && std::env::var_os("TRS_ALLOW_REAL_HOME").is_none()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute() && h.parent().is_some())
}

/// `Err(text)` if `p` is inside the real home directory while the guard is on (a runtime
/// destination such as "Move to..."), with `what` naming it.
pub(crate) fn check_path(what: &str, p: &Path) -> Result<(), String> {
    if !active() {
        return Ok(());
    }
    let Some(home) = home() else { return Ok(()) };
    if p.starts_with(&home) {
        return Err(format!(
            "refusing to use {what} {} inside the real home directory while running under test (set HOME to a temporary folder)",
            p.display()
        ));
    }
    Ok(())
}

/// `Err(text)` if `cfg` makes the engine touch the real home while the guard is on.
pub(crate) fn check_config(cfg: &EngineConfig) -> Result<(), String> {
    if !active() {
        return Ok(());
    }
    let Some(home) = home() else { return Ok(()) };
    let bad = |what: &str, p: &Path| -> Result<(), String> {
        if p.starts_with(&home) {
            Err(format!(
                "refusing to use {what} {} inside the real home directory while running under test; \
                 point the folders into a temporary directory or set HOME (TRS_ALLOW_REAL_HOME=1 overrides)",
                p.display()
            ))
        } else {
            Ok(())
        }
    };
    bad("the download folder", &cfg.default_save_dir)?;
    if let Some(p) = &cfg.incomplete_dir {
        bad("the incomplete folder", p)?;
    }
    let mut needs_trash = cfg.torrent_after_add == AfterAdd::Trash && !cfg.watch.is_empty();
    for w in &cfg.watch {
        bad("the watch folder", &w.path)?;
        if let AfterAdd::Archive(d) = &w.after_add {
            bad("the archive folder", d)?;
        }
        needs_trash |= w.after_add == AfterAdd::Trash;
    }
    if let AfterAdd::Archive(d) = &cfg.torrent_after_add {
        bad("the archive folder", d)?;
    }
    match &cfg.trash_dir {
        Some(t) => bad("the Trash", t)?,
        None if needs_trash => bad("the Trash", &expand_tilde("~/.Trash"))?,
        None => {}
    }
    for (i, l) in cfg.category_locations.iter().enumerate() {
        let name = trs_classify::Category::ALL[i].as_str();
        if let Some(p) = &l.incomplete {
            bad(&format!("the {name} incomplete folder"), p)?;
        }
        if let Some(p) = &l.finished {
            bad(&format!("the {name} finished folder"), p)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_that_point_into_home_are_refused_under_test() {
        let Some(h) = home() else { return };
        let cfg = EngineConfig { default_save_dir: h.join("Downloads"), ..EngineConfig::default() };
        assert!(check_config(&cfg).is_err());
        let ok = EngineConfig { default_save_dir: PathBuf::from("/tmp/x"), ..EngineConfig::default() };
        assert!(check_config(&ok).is_ok());
        // The Trash counts when something is going to be trashed.
        let w = crate::config::WatchConfig {
            path: "/tmp/w".into(),
            mode: crate::config::WatchMode::Auto,
            after_add: AfterAdd::Trash,
            category: None,
            rule: None,
            since: None,
        };
        let trash = EngineConfig { default_save_dir: "/tmp/x".into(), watch: vec![w], ..EngineConfig::default() };
        assert!(check_config(&trash).is_err(), "~/.Trash would be used");
    }
}
