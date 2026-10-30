//! Volume checks for the two locations of a download (SPEC §7A.4).
//!
//! A download is written to its *incomplete* location and moved to its *finished* location when
//! it completes. On one volume that is a `rename`; across volumes it is a copy. Settings shows a
//! warning next to such a pair, and the app asks again whenever a disk is mounted or unmounted,
//! so everything here is a handful of `stat` calls: cheap, no I/O on data, no caches.
//!
//! Volumes are compared by device id (`st_dev`) of the nearest folder that exists, because a
//! location that has not been created yet still lives on the volume of its parent. A path below
//! `/Volumes/<name>` whose volume is not mounted has no nearest folder worth comparing (its
//! parent, `/Volumes`, is the boot volume) and is reported as an error, never as "same volume".

use std::io;
use std::path::{Path, PathBuf};

use trs_classify::Category;

use crate::config::Config;

/// `~` and `~/x` expanded with the home folder; other strings are used as they are.
pub fn expand_tilde(s: &str) -> PathBuf {
    if (s == "~" || s.starts_with("~/"))
        && let Some(home) = trs_platform::dirs::home() {
            let mut p = home;
            if let Some(rest) = s.strip_prefix("~/") {
                p.push(rest);
            }
            return p;
        }
    PathBuf::from(s)
}

/// Device id of the volume `path` is (or would be) on: the nearest existing ancestor's `st_dev`.
///
/// Errors: `NotFound` when the path is below `/Volumes/<name>` and that volume is not mounted,
/// or when no ancestor can be read at all.
pub fn volume_id(path: &Path) -> io::Result<u64> {
    for root in trs_platform::dirs::mount_roots() {
        if let Ok(rest) = path.strip_prefix(root)
            && let Some(first) = rest.components().next()
            && !Path::new(root).join(first).exists()
        {
            return Err(io::Error::new(io::ErrorKind::NotFound, "volume is not mounted"));
        }
    }
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no existing folder");
    for dir in path.ancestors() {
        let probe = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
        match trs_platform::fs::volume_id(probe) {
            Ok(id) => return Ok(id),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Are `a` and `b` on the same volume? See the module docs for how that is decided.
pub fn same_volume(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(volume_id(a)? == volume_id(b)?)
}

/// Which setting a [`CrossVolume`] pair comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationScope {
    /// The app-level defaults (`folders.incomplete_dir` / `folders.download_dir`).
    AppDefault,
    /// A category's effective pair (its override, or the app default where it has none).
    Category(Category),
}

/// An incomplete location and the finished location it feeds that are on different volumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossVolume {
    /// Where the pair is configured.
    pub scope: LocationScope,
    /// The incomplete folder (as written in the settings, `~` unexpanded).
    pub incomplete: String,
    /// The finished folder.
    pub finished: String,
}

/// Every incomplete -> finished pair of `cfg` that crosses volumes, for the Settings warning
/// ("On different disks: finishing a download copies the files instead of an instant move").
///
/// The app-level pair is always checked. A category is checked when it overrides either location;
/// categories that resolve to the same pair as the app default are not repeated.
/// Pairs whose volume cannot be determined (unmounted disk) are left out: there is nothing to
/// warn about until the disk is back, and the caller re-evaluates on mount events.
pub fn cross_volume_pairs(cfg: &Config) -> Vec<CrossVolume> {
    let f = &cfg.folders;
    let mut out = Vec::new();
    let crosses = |inc: &str, fin: &str| -> bool {
        let (i, d) = (expand_tilde(inc), expand_tilde(fin));
        matches!(same_volume(&i, &d), Ok(false))
    };
    if crosses(&f.incomplete_dir, &f.download_dir) {
        out.push(CrossVolume { scope: LocationScope::AppDefault, incomplete: f.incomplete_dir.clone(), finished: f.download_dir.clone() });
    }
    for c in Category::ALL {
        let o = f.categories.get(c);
        let inc = o.incomplete_dir.as_deref().unwrap_or(&f.incomplete_dir);
        let fin = o.finished_dir.as_deref().unwrap_or(&f.download_dir).trim_end_matches('/').to_owned();
        let repeats_app = o.incomplete_dir.is_none() && fin == f.download_dir;
        if !repeats_app && crosses(inc, &fin) {
            out.push(CrossVolume { scope: LocationScope::Category(c), incomplete: inc.to_owned(), finished: fin });
        }
    }
    out
}

/// The pairs of a tracker's own folders (SPEC §7A.4) that cross volumes: the tracker-wide pair
/// (scope `AppDefault` = "this tracker's default") and every category the tracker sets itself.
/// A location the tracker leaves open is the app's (its category's, where it has one).
pub fn tracker_cross_volume_pairs(cfg: &Config, own: &crate::config::LocationOverride, cats: &crate::config::CategoryLocations) -> Vec<CrossVolume> {
    let f = &cfg.folders;
    let crosses = |inc: &str, fin: &str| matches!(same_volume(&expand_tilde(inc), &expand_tilde(fin)), Ok(false));
    let mut out = Vec::new();
    if !own.is_empty() {
        let inc = own.incomplete_dir.as_deref().unwrap_or(&f.incomplete_dir);
        let fin = own.finished_dir.as_deref().unwrap_or(&f.download_dir).trim_end_matches('/').to_owned();
        if crosses(inc, &fin) {
            out.push(CrossVolume { scope: LocationScope::AppDefault, incomplete: inc.to_owned(), finished: fin });
        }
    }
    for c in Category::ALL {
        let o = cats.get(c);
        if o.is_empty() {
            continue;
        }
        let app = f.categories.get(c);
        let inc = o.incomplete_dir.as_deref().or(own.incomplete_dir.as_deref()).or(app.incomplete_dir.as_deref()).unwrap_or(&f.incomplete_dir);
        let fin = o.finished_dir.as_deref().or(own.finished_dir.as_deref()).or(app.finished_dir.as_deref()).unwrap_or(&f.download_dir);
        let fin = fin.trim_end_matches('/').to_owned();
        if crosses(inc, &fin) {
            out.push(CrossVolume { scope: LocationScope::Category(c), incomplete: inc.to_owned(), finished: fin });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder, removed when dropped (also when the test fails).
    struct Tmp(PathBuf);

    impl std::ops::Deref for Tmp {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("trs-vol-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    /// A place on a disk that is not there: below `/Volumes` on Unix, a drive letter nothing uses on Windows.
    fn unmounted(rest: &str) -> String {
        #[cfg(windows)]
        let root = ('D'..='Z').rev().map(|l| format!("{l}:\\")).find(|r| !Path::new(r).exists()).expect("a free drive letter");
        #[cfg(not(windows))]
        let root = "/Volumes/definitely-not-mounted-xyz/".to_string();
        format!("{root}{rest}")
    }

    #[test]
    fn same_directory_tree_is_one_volume_even_when_the_folder_does_not_exist_yet() {
        let d = tmp("same");
        assert!(same_volume(&d, &d.join("not/yet/created")).unwrap());
        assert!(same_volume(&d.join("a/b"), &d.join("c")).unwrap());
    }

    #[test]
    fn an_unmounted_volume_is_an_error_not_a_match() {
        let e = volume_id(Path::new(&unmounted("Media"))).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(same_volume(&std::env::temp_dir(), Path::new(&unmounted("x"))).is_err());
    }

    #[test]
    fn pairs_on_one_volume_are_not_reported() {
        let d = tmp("pairs");
        let mut cfg = Config::default();
        cfg.folders.download_dir = d.join("done").display().to_string();
        cfg.folders.incomplete_dir = d.join("inc").display().to_string();
        assert!(cross_volume_pairs(&cfg).is_empty());
        // A category pointing at a volume that is not mounted is unknown, not "different".
        cfg.folders.categories.movie.finished_dir = Some(unmounted("Movies"));
        assert!(cross_volume_pairs(&cfg).is_empty());
    }

    #[test]
    fn different_devices_are_reported_per_scope() {
        // `/dev` is its own file system on macOS and Linux.
        let (a, b) = (Path::new("/dev"), Path::new("/"));
        if same_volume(a, b).unwrap_or(true) {
            return; // an unusual host: nothing to assert
        }
        let mut cfg = Config::default();
        cfg.folders.incomplete_dir = "/dev/incomplete".into();
        cfg.folders.download_dir = "/".into();
        cfg.folders.categories.tv.finished_dir = Some("/dev/tv".into());
        let pairs = cross_volume_pairs(&cfg);
        assert!(pairs.iter().any(|p| p.scope == LocationScope::AppDefault));
        // The TV override sits next to its incomplete folder (the app default) on /dev: no crossing.
        assert!(!pairs.iter().any(|p| p.scope == LocationScope::Category(Category::Tv)));
    }

    #[test]
    fn a_trackers_folders_are_checked_with_the_apps_for_what_it_leaves_open() {
        let mut cfg = Config::default();
        cfg.folders.download_dir = "/tmp".into();
        cfg.folders.incomplete_dir = "/tmp/inc".into();
        let mut own = crate::config::LocationOverride::default();
        let cats = crate::config::CategoryLocations::default();
        assert!(tracker_cross_volume_pairs(&cfg, &own, &cats).is_empty(), "nothing set: nothing to warn about");
        own.finished_dir = Some(unmounted("T"));
        // An unmounted disk is an error elsewhere, never a "different disks" warning.
        assert!(tracker_cross_volume_pairs(&cfg, &own, &cats).is_empty());
    }
}
