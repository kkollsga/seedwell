//! Organizing finished torrents by sharing state (SPEC 7A.3): the four folders below a finished
//! location, and what the Sharing groups (SPEC 8.3) make of them.

use std::path::{Path, PathBuf};

use crate::api::SeedGroup;

/// A status folder below a finished location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusFolder {
    /// Popular and Active torrents: somebody downloaded from them in the last 24 hours.
    Active,
    /// Nobody asked for 24 hours.
    Dormant,
    /// Paused by the user or stopped at the seeding goal.
    Stopped,
    /// The error has lasted over 24 hours, or the tracker says the torrent is unregistered.
    Dead,
}

impl StatusFolder {
    /// All four, in the order the settings list them.
    pub const ALL: [StatusFolder; 4] = [StatusFolder::Active, StatusFolder::Dormant, StatusFolder::Stopped, StatusFolder::Dead];

    /// The folder's name on disk.
    pub const fn name(self) -> &'static str {
        match self {
            StatusFolder::Active => "Active",
            StatusFolder::Dormant => "Dormant",
            StatusFolder::Stopped => "Stopped",
            StatusFolder::Dead => "Dead",
        }
    }

    fn from_name(n: &std::ffi::OsStr) -> Option<StatusFolder> {
        StatusFolder::ALL.into_iter().find(|f| n == f.name())
    }

    /// Where a finished torrent belongs. `stopped` = paused or its goal reached (then the Sharing
    /// group is `None`). `Error` is transient: `None` = leave the torrent where it is.
    pub fn of(group: Option<SeedGroup>, stopped: bool) -> Option<StatusFolder> {
        if stopped {
            return Some(StatusFolder::Stopped);
        }
        match group? {
            SeedGroup::Popular | SeedGroup::Active => Some(StatusFolder::Active),
            SeedGroup::Dormant => Some(StatusFolder::Dormant),
            SeedGroup::Dead => Some(StatusFolder::Dead),
            SeedGroup::Error => None,
        }
    }
}

/// Split a save path into the finished location and the status folder it is in, if its last
/// component is one.
pub(crate) fn split_status(save: &Path) -> (PathBuf, Option<StatusFolder>) {
    match (save.parent(), save.file_name().and_then(StatusFolder::from_name)) {
        (Some(base), Some(f)) => (base.to_path_buf(), Some(f)),
        _ => (save.to_path_buf(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_map_to_folders() {
        use SeedGroup::*;
        assert_eq!(StatusFolder::of(Some(Popular), false), Some(StatusFolder::Active));
        assert_eq!(StatusFolder::of(Some(Active), false), Some(StatusFolder::Active));
        assert_eq!(StatusFolder::of(Some(Dormant), false), Some(StatusFolder::Dormant));
        assert_eq!(StatusFolder::of(Some(Dead), false), Some(StatusFolder::Dead));
        assert_eq!(StatusFolder::of(Some(Error), false), None, "Error is transient: stay put");
        assert_eq!(StatusFolder::of(None, true), Some(StatusFolder::Stopped));
        assert_eq!(StatusFolder::of(Some(Error), true), Some(StatusFolder::Stopped));
        assert_eq!(StatusFolder::of(None, false), None, "not a seeding torrent");
    }

    #[test]
    fn a_save_path_splits_into_location_and_folder() {
        assert_eq!(split_status(Path::new("/d/Dormant")), (PathBuf::from("/d"), Some(StatusFolder::Dormant)));
        assert_eq!(split_status(Path::new("/d/Movies")), (PathBuf::from("/d/Movies"), None));
        assert_eq!(split_status(Path::new("/d/active")), (PathBuf::from("/d/active"), None), "case matters");
    }
}
