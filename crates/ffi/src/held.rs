//! Cancel on a held torrent (SPEC 7A.6), [`Core::cancel_held`](crate::Core::cancel_held): the torrent is removed and
//! the `.torrent` file it came from goes to the Trash, unless the host keeps it (⇧-Cancel). Rust decides the order
//! (the file is asked for before the cancel, while the engine still knows it), the fallback for a torrent in its
//! countdown, which files may go, where they go and the words of the note when one could not; the host shows the note.

use std::path::{Path, PathBuf};

use trs_platform::fs::{OtherVolume, move_to_trash};
use trs_platform::{shift_key, trash};

use crate::backend::Backend;
use crate::menu::APP_NAME;

/// What a Cancel leaves for the host to say: a quiet notification, never an alert.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct CancelNote {
    /// "Couldn't move a.torrent to the Trash".
    pub title: String,
    /// "The torrent was removed."
    pub body: String,
}

/// Cancels the held torrent `id` on `backend`; the note to show when its `.torrent` file could not go to the Trash.
pub(crate) fn cancel_held(backend: &dyn Backend, id: &str, keep_file: bool) -> Option<CancelNote> {
    // Asked before the cancel: the engine forgets the file when the torrent goes. It re-reads the file now and checks
    // that it is a regular `.torrent` file with the torrent's own info-hash.
    let source = if keep_file { None } else { backend.held_source(id) };
    if backend.cancel_choice(id).is_err() {
        // A torrent in its countdown does not wait for a choice: the engine will not cancel it, so Cancel removes it
        // (nothing was downloaded; no files are deleted).
        backend.remove(&[id.to_owned()], false);
    }
    let path = PathBuf::from(source?);
    let trash = trash_dir(backend);
    trash_source(&path, trash.as_deref()).err().map(|()| CancelNote {
        title: format!(concat!("Couldn't move {} to the ", trash!()), file_name(&path)),
        body: "The torrent was removed.".into(),
    })
}

/// The hover text of Cancel on a held torrent: with a `.torrent` file to trash (`has_source_file`), both things it does.
#[uniffi::export]
pub fn cancel_help(has_source_file: bool) -> String {
    if has_source_file {
        concat!("Remove this torrent and move its .torrent file to the ", trash!(), ". Hold ", shift_key!(), " to keep the file.").into()
    } else {
        "Remove this torrent. Nothing has been downloaded".into()
    }
}

/// What the chooser window asks before its Cancel removes a held torrent, with every text it shows.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct CancelQuestion {
    /// "Remove “Sintel”?"
    pub title: String,
    /// What happens: nothing was downloaded, and where the `.torrent` file goes.
    pub text: String,
    /// "Remove": removes it (then `cancel_held`).
    pub remove_title: String,
    /// "Keep Waiting": nothing happens.
    pub keep_title: String,
}

/// The question for Cancel on the held torrent `name`: its `.torrent` file goes to the Trash unless `keep_file`
/// (⇧-Cancel), and nothing is said of a file when there is none (`has_source_file`).
#[uniffi::export]
pub fn cancel_question(name: String, has_source_file: bool, keep_file: bool) -> CancelQuestion {
    let file = match (has_source_file, keep_file) {
        (false, _) => "",
        (true, true) => " Its .torrent file stays where it is.",
        (true, false) => concat!(" Its .torrent file moves to the ", trash!(), "."),
    };
    CancelQuestion {
        title: format!("Remove “{name}”?"),
        text: format!("Nothing has been downloaded. It is removed from {APP_NAME}.{file}"),
        remove_title: "Remove".into(),
        keep_title: "Keep Waiting".into(),
    }
}

/// Moves a `.torrent` file to the Trash (`trash_dir`, else the system's). Only a regular file (never a link or a
/// folder) named `*.torrent`; anything else is left alone without complaint. `Err` when the move failed.
fn trash_source(path: &Path, trash_dir: Option<&Path>) -> Result<(), ()> {
    let regular = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
    let named = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent"));
    if !(regular && named) {
        return Ok(());
    }
    move_to_trash(path, trash_dir, OtherVolume::Copy).map(|_| ()).map_err(|_| ())
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Where Cancel puts the file, and a removed synced folder goes ([`crate::sync_removal`]): the QA folder
/// (`SEEDWELL_QA_TRASH_DIR`, honoured only inside a temporary folder: the real Trash must not receive QA files, and the
/// switch must not point at the user's own), else the backend's stand-in, else (`None`) the system's Trash.
pub(crate) fn trash_dir(backend: &dyn Backend) -> Option<PathBuf> {
    let qa = std::env::var_os("SEEDWELL_QA_TRASH_DIR").filter(|d| !d.is_empty()).map(PathBuf::from);
    let dir = qa.filter(|d| trs_platform::dirs::in_temp_folder(d)).or_else(|| backend.trash_dir());
    #[cfg(test)]
    assert!(dir.is_some(), "a test would use the real Trash");
    dir
}

/// What Start does with a torrent that waits for a choice when nobody ticked anything (SPEC 7A.6), and what its
/// chooser shows ticked: every file of the chooser's items except the Extras group's (samples, featurettes, .nfo).
/// One flag per torrent file index; files in no item are not wanted.
#[uniffi::export]
pub fn choice_default_files(info: &crate::types::ChoiceInfo) -> Vec<bool> {
    let mut flags = vec![false; info.file_count as usize];
    for g in info.groups.iter().filter(|g| g.kind != crate::types::ChoiceGroupKind::Extras) {
        for f in g.items.iter().flat_map(|i| &i.files) {
            if let Some(flag) = flags.get_mut(*f as usize) {
                *flag = true;
            }
        }
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mock, its state folder (whose `Trash` folder stands in for the Trash), and a `.torrent` file it holds.
    fn held(name: &str) -> (std::sync::Arc<dyn Backend>, tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let b = crate::mock::MockBackend::start(dir.path().join("state").display().to_string());
        let f = dir.path().join(name);
        std::fs::write(&f, b"d4:infode").unwrap();
        let id = b.add_source(&f.display().to_string()).unwrap();
        assert!(b.has_held_source(&id), "the mock knows the file");
        (b, dir, f, id)
    }

    #[test]
    fn cancel_removes_the_torrent_and_puts_its_file_in_the_trash() {
        let (b, dir, f, id) = held("a.torrent");
        assert_eq!(cancel_held(&*b, &id, false), None, "no note");
        assert!(b.info(&id).is_err(), "removed (the mock's cancel fails as a countdown's does: removed instead)");
        assert!(!f.exists());
        assert!(dir.path().join("state/Trash/a.torrent").is_file());
        // A second file of that name does not replace the first.
        std::fs::write(&f, b"d4:infode").unwrap();
        let id = b.add_source(&f.display().to_string()).unwrap();
        assert_eq!(cancel_held(&*b, &id, false), None);
        let trash = dir.path().join("state/Trash");
        assert!(trash.join("a (2).torrent").is_file() && trash.join("a.torrent").is_file());
    }

    #[test]
    fn shift_cancel_keeps_the_file_and_a_magnet_link_has_none() {
        let (b, dir, f, id) = held("a.torrent");
        assert_eq!(cancel_held(&*b, &id, true), None);
        assert!(b.info(&id).is_err() && f.is_file(), "removed, the file kept");
        assert!(!dir.path().join("state/Trash").exists());
        let m = b.add_source("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Test").unwrap();
        assert!(!b.has_held_source(&m));
        assert_eq!(cancel_held(&*b, &m, false), None);
        assert!(b.info(&m).is_err());
    }

    #[test]
    fn a_file_that_cannot_be_moved_never_stops_the_removal_and_leaves_a_quiet_note() {
        let (b, dir, f, id) = held("a.torrent");
        // The stand-in Trash is a file: nothing can go in it.
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        std::fs::write(dir.path().join("state/Trash"), b"").unwrap();
        let note = cancel_held(&*b, &id, false).expect("a note");
        assert_eq!(note.title, concat!("Couldn't move a.torrent to the ", trash!()));
        assert_eq!(note.body, "The torrent was removed.");
        assert!(b.info(&id).is_err() && f.is_file(), "removed; the file stays where it was");
    }

    #[test]
    fn only_a_plain_torrent_file_is_ever_moved() {
        let dir = tempfile::tempdir().unwrap();
        let trash = dir.path().join("Trash");
        let other = dir.path().join("movie.mkv");
        std::fs::write(&other, b"x").unwrap();
        assert_eq!(trash_source(&other, Some(&trash)), Ok(()), "left alone without a note");
        let folder = dir.path().join("folder.torrent");
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(trash_source(&folder, Some(&trash)), Ok(()));
        assert_eq!(trash_source(&dir.path().join("gone.torrent"), Some(&trash)), Ok(()));
        #[cfg(unix)]
        {
            let link = dir.path().join("link.torrent");
            std::os::unix::fs::symlink(&other, &link).unwrap();
            assert_eq!(trash_source(&link, Some(&trash)), Ok(()));
            assert!(link.exists());
        }
        assert!(other.is_file() && folder.is_dir() && !trash.exists(), "nothing moved");
        let upper = dir.path().join("Y.TORRENT");
        std::fs::write(&upper, b"x").unwrap();
        assert_eq!(trash_source(&upper, Some(&trash)), Ok(()));
        assert!(trash.join("Y.TORRENT").is_file(), "the extension in any case");
    }

    #[test]
    fn the_words_of_cancel_name_the_file_only_when_there_is_one() {
        assert_eq!(cancel_help(true), concat!("Remove this torrent and move its .torrent file to the ", trash!(), ". Hold ", shift_key!(), " to keep the file."));
        assert_eq!(cancel_help(false), "Remove this torrent. Nothing has been downloaded");
        let q = cancel_question("Sintel".into(), true, false);
        assert_eq!(q.title, "Remove “Sintel”?");
        assert_eq!(q.text, concat!("Nothing has been downloaded. It is removed from Seedwell. Its .torrent file moves to the ", trash!(), "."));
        assert_eq!((q.remove_title.as_str(), q.keep_title.as_str()), ("Remove", "Keep Waiting"));
        assert_eq!(cancel_question("Sintel".into(), true, true).text, "Nothing has been downloaded. It is removed from Seedwell. Its .torrent file stays where it is.");
        assert_eq!(cancel_question("Sintel".into(), false, false).text, "Nothing has been downloaded. It is removed from Seedwell.");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_mac_words_are_the_ones_it_always_had() {
        assert_eq!(cancel_help(true), "Remove this torrent and move its .torrent file to the Trash. Hold ⇧ to keep the file.");
        assert_eq!(cancel_question("Sintel".into(), true, false).text, "Nothing has been downloaded. It is removed from Seedwell. Its .torrent file moves to the Trash.");
    }

    #[test]
    fn a_file_the_engine_no_longer_vouches_for_stays() {
        let (b, dir, f, id) = held("a.torrent");
        // Replaced by a link: the mock, like the engine, no longer offers it.
        std::fs::remove_file(&f).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), &f).unwrap();
        assert!(!b.has_held_source(&id));
        assert_eq!(cancel_held(&*b, &id, false), None);
        assert!(!dir.path().join("state/Trash").exists());
    }

    #[test]
    fn start_without_a_choice_takes_everything_but_the_extras() {
        use crate::types::{Category, ChoiceGroupInfo, ChoiceGroupKind, ChoiceInfo, ChoiceItemInfo};
        let item = |files: &[u32]| ChoiceItemInfo {
            label: "x".into(), bytes: 1, files: files.to_vec(), selected: false, in_library: true, other_quality: false,
            quality: None, this_mac: false,
        };
        let group = |kind, items| ChoiceGroupInfo { label: "g".into(), kind, items };
        let info = ChoiceInfo {
            id: "t".into(), name: "n".into(), display_name: "n".into(), tracker: String::new(), awaiting: true,
            reasons: vec![], unsure: false, structure: "tv".into(),
            groups: vec![
                group(ChoiceGroupKind::Season, vec![item(&[0, 1]), item(&[2])]),
                group(ChoiceGroupKind::Extras, vec![item(&[3]), item(&[4])]),
                group(ChoiceGroupKind::Other, vec![item(&[5, 99])]),
            ],
            file_count: 7, total_bytes: 7, selected_bytes: 0, free_bytes: None, destination: None,
            category: Category::Tv, confidence: 1.0, library_checked: true, rule_name: String::new(),
            then_text: String::new(), summary_text: String::new(),
        };
        // Already in the library or not, preselected or not: the default is the same (the chooser shows it).
        assert_eq!(choice_default_files(&info), vec![true, true, true, false, false, true, false], "file 6 is in no item");
    }
}
