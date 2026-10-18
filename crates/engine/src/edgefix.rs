//! Edge files: the hidden partial (`.seedwell-partial/<name>`, or `.dispatch-partial/<name>` from
//! older builds) and the clean destination of one torrent file.
//!
//! A file only ever lives under one of these names. Two things can break that, and both are
//! repaired when the torrent starts (`spawn_torrent`, so also on a recheck):
//!
//! * **A crash between the rename and the record.** The record names one path, the data is at its
//!   sibling. [`reconcile`] renames the sibling to the recorded path.
//! * **Both exist.** Builds before the rename was made atomic with the layout switch could leave
//!   the real data at the hidden path while the visible path held only the bytes of the edge
//!   pieces (zeros elsewhere), and the torrent seeded "complete" from it. [`reconcile`] reports
//!   these as [`Conflict`]s; the actor then rechecks the torrent against both candidates and
//!   [`settle`] keeps the one with more verifying pieces at the recorded path. The loser is moved
//!   aside to `<hidden folder>/<name>.stale`, never deleted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::organize;
use crate::persist::Placement;

/// Both names of one file exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Conflict {
    pub file: usize,
    /// The path the record names.
    pub cur: PathBuf,
    /// The other name, which also exists.
    pub alt: PathBuf,
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Looks at every placed file once (stat only). A recorded path that is missing while a sibling
/// exists is repaired by renaming the sibling; files that exist under two names are returned.
pub(crate) fn reconcile(pl: &Placement) -> Vec<Conflict> {
    let mut out = Vec::new();
    // One `stat` per folder tells whether it has a hidden folder at all.
    let mut has_hidden: HashMap<PathBuf, bool> = HashMap::new();
    for (i, cur) in pl.files.iter().enumerate() {
        let Some(cur) = cur else { continue };
        let [proper, ..] = organize::edge_names(cur);
        let Some(dir) = proper.parent() else { continue };
        let hidden = *has_hidden.entry(dir.to_path_buf()).or_insert_with(|| {
            exists(&dir.join(organize::EDGE_DIR)) || exists(&dir.join(organize::LEGACY_EDGE_DIR))
        });
        if !hidden {
            continue;
        }
        let alts = organize::edge_names(cur).into_iter().filter(|p| p != cur);
        if exists(cur) {
            if let Some(alt) = alts.into_iter().find(|p| exists(p)) {
                out.push(Conflict {
                    file: i,
                    cur: cur.clone(),
                    alt,
                });
            }
        } else if let Some(alt) = alts.into_iter().find(|p| exists(p)) {
            if let Some(d) = cur.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = trs_storage::rename_excl(&alt, cur);
        }
    }
    out
}

/// A free name for a file set aside: `<hidden folder>/<name>.stale[.N]`.
pub(crate) fn stale_path(proper: &Path) -> PathBuf {
    let dir = proper
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(organize::EDGE_DIR);
    let name = proper
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut p = dir.join(format!("{name}.stale"));
    let mut n = 1;
    while exists(&p) {
        p = dir.join(format!("{name}.stale.{n}"));
        n += 1;
    }
    p
}

/// Puts the winner at `c.cur`. `keep_cur`: the recorded path verified at least as well as the
/// other name. The loser is moved aside, not deleted.
pub(crate) fn settle(c: &Conflict, keep_cur: bool) -> std::io::Result<()> {
    let [proper, ..] = organize::edge_names(&c.cur);
    let aside = |from: &Path| -> std::io::Result<()> {
        let to = stale_path(&proper);
        if let Some(d) = to.parent() {
            std::fs::create_dir_all(d)?;
        }
        trs_storage::rename_excl(from, &to)
    };
    if keep_cur {
        aside(&c.alt)
    } else {
        aside(&c.cur)?;
        trs_storage::rename_excl(&c.alt, &c.cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pl(files: Vec<PathBuf>) -> Placement {
        Placement {
            files: files.into_iter().map(Some).collect(),
            library_root: PathBuf::new(),
        }
    }

    fn put(p: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    #[test]
    fn a_sibling_is_renamed_to_the_recorded_path_when_that_is_missing() {
        let d = tempfile::tempdir().unwrap();
        let proper = d.path().join("S/Season 01/E03.mkv");
        let [_, hidden, legacy] = organize::edge_names(&proper);
        // Record says clean, data hidden (crash after the record, before the rename).
        put(&hidden, b"data");
        assert!(reconcile(&pl(vec![proper.clone()])).is_empty());
        assert_eq!(std::fs::read(&proper).unwrap(), b"data");
        assert!(!hidden.exists());
        // Record says hidden, data at the clean path (crash after the rename, before the record).
        std::fs::remove_file(&proper).unwrap();
        put(&proper, b"more");
        assert!(reconcile(&pl(vec![hidden.clone()])).is_empty());
        assert_eq!(std::fs::read(&hidden).unwrap(), b"more");
        // A record from an older build names the legacy folder; data sits at the clean path.
        std::fs::remove_file(&hidden).unwrap();
        put(&proper, b"old");
        assert!(reconcile(&pl(vec![legacy.clone()])).is_empty());
        assert_eq!(std::fs::read(&legacy).unwrap(), b"old");
    }

    #[test]
    fn two_names_that_exist_are_reported_and_settled_without_deleting_anything() {
        let d = tempfile::tempdir().unwrap();
        let proper = d.path().join("S/Season 01/E03.mkv");
        let [_, hidden, _] = organize::edge_names(&proper);
        put(&proper, b"zeros");
        put(&hidden, b"real");
        let c = reconcile(&pl(vec![proper.clone()]));
        assert_eq!(
            c,
            vec![Conflict {
                file: 0,
                cur: proper.clone(),
                alt: hidden.clone()
            }]
        );
        // The hidden one verifies better: it takes the recorded path.
        let stale = stale_path(&proper);
        settle(&c[0], false).unwrap();
        assert_eq!(std::fs::read(&proper).unwrap(), b"real");
        assert!(!hidden.exists());
        assert_eq!(std::fs::read(&stale).unwrap(), b"zeros");
        // The recorded one wins: the other goes aside.
        put(&hidden, b"junk");
        let c = reconcile(&pl(vec![proper.clone()]));
        settle(&c[0], true).unwrap();
        assert_eq!(std::fs::read(&proper).unwrap(), b"real");
        assert!(!hidden.exists());
        let second = stale.with_file_name("E03.mkv.stale.1");
        assert_eq!(std::fs::read(second).unwrap(), b"junk", "a second loser gets its own name");
    }
}
