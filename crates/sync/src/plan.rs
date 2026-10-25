//! The planner: from what a folder needs to the order the puller works in. Pure.
//!
//! Order (SPEC §7G, design §5.2): absorbed deletes (index only), directories (shallowest first), renames,
//! files (smallest first, so a folder of small documents is usable before a big video arrives), then
//! deletes (deepest first, so a directory is empty by the time its own delete comes). The puller re-plans
//! after every round, so a step that could not run yet (a directory that is not empty) simply comes again.

use trs_sync_proto::model::{FileInfo, FileKind};

use crate::global::Need;
use crate::index::{LocalRecord, NeedRecord};

/// One thing for the puller to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Create a directory (or replace a file there with one).
    MkDir(String),
    /// Move our copy from `from` to `to` instead of fetching `to`: the content is the same, and `from`
    /// is deleted in the current version.
    Rename {
        /// The path we have, which is deleted now.
        from: String,
        /// The path to take, with the same content.
        to: String,
    },
    /// Build a file or symlink from local chunks and peers.
    Fetch(String),
    /// Delete a file, symlink or (empty) directory.
    Delete(String),
    /// Record a new version of what we hold that has seen a delete that lost to it (index only).
    Absorb(String),
}

fn depth(p: &str) -> usize {
    p.bytes().filter(|&b| b == b'/').count()
}

/// Same content and kind, so a rename can stand in for a fetch.
fn same_file(a: &FileInfo, b: &FileInfo) -> bool {
    a.kind == FileKind::File && b.kind == FileKind::File && a.exec == b.exec && a.size == b.size && a.chunks == b.chunks
}

/// Orders `needs` into steps. `local` looks up our current record of a path.
pub fn plan(needs: &[(String, NeedRecord)], local: &dyn Fn(&str) -> Option<LocalRecord>) -> Vec<Step> {
    let mut dirs = Vec::new();
    let mut files: Vec<(&str, &FileInfo)> = Vec::new();
    let mut deletes: Vec<(&str, Option<LocalRecord>)> = Vec::new();
    let mut absorbs = Vec::new();
    for (path, n) in needs {
        let info = &n.file.info;
        if n.need == Need::Absorb {
            absorbs.push(Step::Absorb(path.clone()));
        } else if info.deleted {
            // A delete of something we hold, or a newer tombstone over ours (the step then only records it).
            let held = local(path).filter(|l| !l.file.info.deleted);
            if held.is_some() || n.need != Need::Nothing {
                deletes.push((path, held));
            }
        } else if info.kind == FileKind::Dir {
            dirs.push(path.as_str());
        } else {
            files.push((path, info));
        }
    }

    // Renames: a file to take whose exact content we hold at a path that is being deleted, and which we
    // have nothing at yet. Each deleted path stands in once.
    let mut steps: Vec<Step> = Vec::with_capacity(needs.len());
    steps.append(&mut absorbs);
    dirs.sort_by_key(|p| (depth(p), *p));
    steps.extend(dirs.into_iter().map(|p| Step::MkDir(p.to_string())));

    let mut used = vec![false; deletes.len()];
    let mut fetches = Vec::new();
    files.sort_by_key(|&(p, info)| (info.size, p));
    for (path, wanted) in files {
        // A read-only device's own file over a received delete is something: it is not moved over.
        let have_nothing = local(path).is_none_or(|l| !crate::pull::holds(&l));
        let stand_in = have_nothing
            .then(|| {
                deletes.iter().enumerate().position(|(i, (_, l))| {
                    !used[i] && l.as_ref().is_some_and(|l| !l.invalid && same_file(&l.file.info, wanted))
                })
            })
            .flatten();
        match stand_in {
            Some(i) => {
                used[i] = true;
                steps.push(Step::Rename { from: deletes[i].0.to_string(), to: path.to_string() });
            }
            None => fetches.push(Step::Fetch(path.to_string())),
        }
    }
    steps.extend(fetches);

    let mut rest: Vec<&str> =
        deletes.iter().enumerate().filter(|(i, _)| !used[*i]).map(|(_, (p, _))| *p).collect();
    rest.sort_by_key(|p| (std::cmp::Reverse(depth(p)), *p));
    steps.extend(rest.into_iter().map(|p| Step::Delete(p.to_string())));
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use trs_sync_proto::ids::ShortId;
    use trs_sync_proto::model::{ChunkRef, SignedFile, Version};

    fn info(path: &str, kind: FileKind, size: u64, content: u8, deleted: bool) -> FileInfo {
        FileInfo {
            path: path.into(),
            kind,
            size,
            mtime_ns: 0,
            exec: false,
            deleted,
            version: Version::new(),
            modified_by: ShortId(1),
            file_id: [0; 16],
            symlink_target: String::new(),
            conflict_of: None,
            chunks: if kind == FileKind::File && !deleted { vec![ChunkRef { len: size as u32, hash: [content; 32] }] } else { vec![] },
        }
    }

    fn need(i: FileInfo) -> (String, NeedRecord) {
        (i.path.clone(), NeedRecord { need: Need::Take, file: SignedFile { info: i, sig: [0; 64] }, holders: vec![] })
    }

    fn rec(i: FileInfo) -> LocalRecord {
        LocalRecord { file: SignedFile { info: i, sig: [0; 64] }, sequence: 1, invalid: false, stamp: None, tier: None }
    }

    #[test]
    fn dirs_then_files_by_size_then_deletes_deepest_first() {
        let needs = vec![
            need(info("b/big", FileKind::File, 100, 1, false)),
            need(info("b", FileKind::Dir, 0, 0, false)),
            need(info("a/x/y", FileKind::Dir, 0, 0, false)),
            need(info("a", FileKind::Dir, 0, 0, false)),
            need(info("small", FileKind::File, 1, 2, false)),
            need(info("old", FileKind::Dir, 0, 0, true)),
            need(info("old/f", FileKind::File, 0, 0, true)),
        ];
        let have = |p: &str| match p {
            "old" => Some(rec(info("old", FileKind::Dir, 0, 0, false))),
            "old/f" => Some(rec(info("old/f", FileKind::File, 5, 9, false))),
            _ => None,
        };
        let steps = plan(&needs, &have);
        assert_eq!(
            steps,
            vec![
                Step::MkDir("a".into()),
                Step::MkDir("b".into()),
                Step::MkDir("a/x/y".into()),
                Step::Fetch("small".into()),
                Step::Fetch("b/big".into()),
                Step::Delete("old/f".into()),
                Step::Delete("old".into()),
            ]
        );
    }

    #[test]
    fn a_rename_elsewhere_moves_our_copy_instead_of_fetching() {
        let needs = vec![need(info("new", FileKind::File, 10, 7, false)), need(info("old", FileKind::File, 0, 0, true))];
        let have = |p: &str| (p == "old").then(|| rec(info("old", FileKind::File, 10, 7, false)));
        assert_eq!(plan(&needs, &have), vec![Step::Rename { from: "old".into(), to: "new".into() }]);
    }

    #[test]
    fn different_content_is_fetched_and_the_old_path_deleted() {
        let needs = vec![need(info("new", FileKind::File, 10, 7, false)), need(info("old", FileKind::File, 0, 0, true))];
        let have = |p: &str| (p == "old").then(|| rec(info("old", FileKind::File, 10, 8, false)));
        assert_eq!(plan(&needs, &have), vec![Step::Fetch("new".into()), Step::Delete("old".into())]);
    }

    #[test]
    fn a_tombstone_of_something_we_never_had_is_no_step() {
        let needs = vec![(
            "gone".to_string(),
            NeedRecord { need: Need::Nothing, file: SignedFile { info: info("gone", FileKind::File, 0, 0, true), sig: [0; 64] }, holders: vec![] },
        )];
        assert!(plan(&needs, &|_| None).is_empty());
    }
}
