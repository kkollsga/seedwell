//! Removing a synced folder (SPEC 7G.5), [`Core::sync_remove`](crate::Core::sync_remove): it stops syncing on this
//! computer and, when asked, the folder goes to the Trash (the Recycle Bin on Windows) through the Trash seam. Rust
//! decides the order (the Trash is checked before anything stops, so a folder that cannot go there stays synced), where
//! it goes (the QA folder, the backend's stand-in or the system's) and the words of the refusal and of the alert when
//! the move fails afterwards; the host only calls it and shows what comes back.

use std::path::Path;

use trs_platform::fs::{OtherVolume, move_to_trash, trash_check};

use crate::backend::Backend;
use crate::held::trash_dir;
use crate::sync::SyncBackend;
use crate::sync_text::{SyncAlert, sync_trash_failed, sync_trash_refusal};
use crate::types::CoreError;

/// Removes the folder `id` from sync on `backend`, and with `trash_files` moves it to the Trash.
pub(crate) fn sync_remove(backend: &dyn Backend, id: &str, trash_files: bool) -> Result<Option<SyncAlert>, CoreError> {
    let trash = trash_files.then(|| trash_dir(backend));
    remove(&*backend.sync(), id, trash.as_ref().map(Option::as_deref))
}

/// `trash`: `None` keeps the files; `Some(dir)` moves the folder into `dir`, or the system's Trash when that is `None`.
fn remove(sync: &dyn SyncBackend, id: &str, trash: Option<Option<&Path>>) -> Result<Option<SyncAlert>, CoreError> {
    // A folder is never copied into a Trash folder: with one (a QA or stand-in folder) a folder on another volume is
    // refused; the Mac's own Trash keeps it on its own volume; the Recycle Bin refuses one it would delete at once.
    // Asked now, while nothing has changed yet.
    // The check is made where the files go from: an on-demand folder's cache (they go to the Trash from beside it,
    // under the folder's name, never by way of its place).
    let folder = match trash {
        Some(dir) => sync.snapshot().folders.into_iter().find(|f| f.id == id).map(|f| {
            let files = sync.files_path(id).unwrap_or_else(|| f.path.clone());
            let path = Path::new(&files);
            if on_disk(path) {
                trash_check(path, dir, OtherVolume::Refuse).map_err(|e| CoreError::Failed { message: sync_trash_refusal(&f.name, &e) })?;
            }
            Ok::<_, CoreError>((f.name, f.path))
        }),
        None => None,
    }
    .transpose()?;
    // Without the Trash, or a folder not listed (nothing left on disk to move).
    let (Some(dir), Some((name, place))) = (trash, folder) else {
        sync.remove(id)?;
        return Ok(None);
    };
    let files = sync.remove_for_trash(id)?;
    let path = Path::new(&files);
    if !on_disk(path) {
        return Ok(None);
    }
    let trashed = move_to_trash(path, dir, OtherVolume::Refuse).is_ok();
    // Refused: an on-demand folder's files are left at its place, as an ordinary folder.
    let at = sync.finish_removal(&files, &place, trashed)?;
    Ok((!trashed).then(|| sync_trash_failed(name, trs_platform::dirs::shown(Path::new(&at)))))
}

/// Something is there (a link counts: the link itself goes, never what it points at).
fn on_disk(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use trs_platform::{computer, trash};

    use super::*;

    /// The mock, its state folder (whose `Trash` folder stands in for the Trash), and a synced folder `Photos` with a
    /// file in it, and its id.
    fn synced() -> (Arc<dyn Backend>, tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let b = crate::mock::MockBackend::start(dir.path().join("state").display().to_string());
        let folder = dir.path().join("Photos");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a.jpg"), b"x").unwrap();
        let id = b.sync().add_folder(&folder.display().to_string()).unwrap().id;
        (b, dir, folder, id)
    }

    fn is_synced(b: &dyn Backend, id: &str) -> bool {
        b.sync().snapshot().folders.iter().any(|f| f.id == id)
    }

    #[test]
    fn a_removed_folder_goes_to_the_trash_when_asked() {
        let (b, dir, folder, id) = synced();
        assert_eq!(sync_remove(&*b, &id, true).unwrap(), None, "nothing to say");
        assert!(!is_synced(&*b, &id) && !folder.exists());
        assert!(dir.path().join("state/Trash/Photos/a.jpg").is_file(), "in the mock's own Trash");
        // Another folder of that name does not replace the first.
        std::fs::create_dir_all(&folder).unwrap();
        let id = b.sync().add_folder(&folder.display().to_string()).unwrap().id;
        assert_eq!(sync_remove(&*b, &id, true).unwrap(), None);
        assert!(dir.path().join("state/Trash/Photos 2").is_dir() && dir.path().join("state/Trash/Photos").is_dir());
    }

    #[test]
    fn without_the_trash_the_files_stay() {
        let (b, dir, folder, id) = synced();
        assert_eq!(sync_remove(&*b, &id, false).unwrap(), None);
        assert!(!is_synced(&*b, &id) && folder.join("a.jpg").is_file());
        assert!(!dir.path().join("state/Trash").exists());
        let e = sync_remove(&*b, &id, true).unwrap_err();
        assert_eq!(e.to_string(), concat!("This folder is no longer synced on this ", computer!(), "."));
    }

    #[test]
    fn a_folder_no_longer_on_disk_just_stops_syncing() {
        let (b, dir, folder, id) = synced();
        std::fs::remove_dir_all(&folder).unwrap();
        assert_eq!(sync_remove(&*b, &id, true).unwrap(), None);
        assert!(!is_synced(&*b, &id) && !dir.path().join("state/Trash").exists());
    }

    /// A Trash folder on another volume (devfs) refuses the folder before anything stops: it stays synced and in place.
    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_go_to_the_trash_stays_synced_and_the_error_says_what_to_do() {
        use trs_platform::file_manager;
        let (b, _dir, folder, id) = synced();
        let elsewhere = Path::new("/dev/seedwell-test-trash");
        let e = remove(&*b.sync(), &id, Some(Some(elsewhere))).unwrap_err();
        let CoreError::Failed { message } = e else { panic!("{e:?}") };
        assert_eq!(
            message,
            concat!(
                "“Photos” is on another volume than the ", trash!(), ", so it can't be moved there. Turn off “Also move the files to the ",
                trash!(), "” to stop syncing it, then delete the folder in ", file_manager!(), "."
            )
        );
        assert!(is_synced(&*b, &id) && folder.join("a.jpg").is_file());
    }

    #[test]
    fn a_move_that_fails_after_the_folder_stopped_syncing_leaves_an_alert() {
        let (b, dir, folder, id) = synced();
        // The stand-in Trash is a file: the check finds a free name in it, the move cannot make the folder.
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        std::fs::write(dir.path().join("state/Trash"), b"").unwrap();
        let alert = sync_remove(&*b, &id, true).unwrap().expect("an alert");
        assert_eq!(alert.title, concat!("Couldn't move “Photos” to the ", trash!()));
        let shown = trs_platform::dirs::shown(&folder);
        assert_eq!(alert.text, format!(concat!("It is no longer synced on this ", computer!(), ". Its files are still in {}."), shown));
        assert!(!is_synced(&*b, &id) && folder.join("a.jpg").is_file(), "stopped; the files stay where they were");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_mac_words_are_the_ones_it_always_had() {
        let (b, dir, folder, id) = synced();
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        std::fs::write(dir.path().join("state/Trash"), b"").unwrap();
        let alert = sync_remove(&*b, &id, true).unwrap().unwrap();
        assert_eq!(alert.title, "Couldn't move “Photos” to the Trash");
        assert_eq!(alert.text, format!("It is no longer synced on this Mac. Its files are still in {}.", trs_platform::dirs::shown(&folder)));
        let (b, _dir, _folder, id) = synced();
        let e = remove(&*b.sync(), &id, Some(Some(Path::new("/dev/seedwell-test-trash")))).unwrap_err();
        assert_eq!(
            e.to_string(),
            "“Photos” is on another volume than the Trash, so it can't be moved there. Turn off “Also move the files to the Trash” to stop syncing it, then delete the folder in Finder."
        );
    }
}
