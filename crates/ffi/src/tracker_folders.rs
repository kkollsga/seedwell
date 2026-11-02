//! A favorite tracker's own folders (SPEC §7A.4, §7E.6), as the folder editor every host draws on the tracker's Info
//! page and in Settings › Rules › a tracker (docs/CROSS-PLATFORM.md §1): its Incomplete and Finished folders, and some
//! for a content type. An empty field is "Same as Settings". The host draws [`tracker_folders_view`] and hands each
//! change to [`tracker_folders_set`], which returns the record to save (or to keep as the Info page's draft).

use crate::row_text::category_label;
use crate::settings::{CategoryLocationSetting, ProfileRecord, VolumeWarning};
use crate::settings_view::settings_different_disks;
use crate::types::Category;

/// The content types, in the order the editor lists them.
const TYPES: [Category; 8] =
    [Category::Movie, Category::Tv, Category::Music, Category::Audiobook, Category::Book, Category::Software, Category::Game, Category::Other];

/// Which of a pair of folders a field edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TrackerFolderField {
    Incomplete,
    Finished,
}

/// One folder field: its label, the folder (`None`: the field is empty and says `placeholder`), and the help of the
/// field, of Choose… and of the button that empties it (shown only while there is a folder).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerFolderRow {
    /// Which folder of the pair, and of which content type (`None`: the tracker's own pair).
    pub category: Option<Category>,
    pub field: TrackerFolderField,
    pub label: String,
    pub path: Option<String>,
    pub placeholder: String,
    pub help: String,
    pub choose_title: String,
    pub choose_help: String,
    pub clear_help: String,
}

/// A content type's pair of folders, under its name.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerFolderType {
    pub category: Category,
    pub title: String,
    /// "custom folders", after the name, while the type has a folder of its own.
    pub custom: Option<String>,
    /// The type's folders are on different disks: a warning sign with this help.
    pub warning: Option<String>,
    pub rows: Vec<TrackerFolderRow>,
}

/// The editor, top to bottom: the tracker's pair, its different-disk warning, the note, and the content types in a
/// disclosure.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerFoldersView {
    pub rows: Vec<TrackerFolderRow>,
    /// The tracker's own pair is on different disks: the orange warning under it.
    pub warning: Option<String>,
    /// Quiet, under the pair.
    pub note: String,
    pub types_title: String,
    /// The disclosure of the content types starts open (a type has folders of its own).
    pub types_open: bool,
    pub types: Vec<TrackerFolderType>,
}

/// A type's folders (both `None` when the tracker sets none for it).
fn pair(r: &ProfileRecord, category: Option<Category>) -> (Option<String>, Option<String>) {
    match category {
        None => (r.incomplete_dir.clone(), r.finished_dir.clone()),
        Some(c) => r.categories.iter().find(|s| s.category == c).map_or((None, None), |s| (s.incomplete_dir.clone(), s.finished_dir.clone())),
    }
}

fn rows(r: &ProfileRecord, category: Option<Category>) -> Vec<TrackerFolderRow> {
    let (incomplete, finished) = pair(r, category);
    [(TrackerFolderField::Incomplete, "Incomplete", incomplete), (TrackerFolderField::Finished, "Finished", finished)]
        .into_iter()
        .map(|(field, label, path)| TrackerFolderRow {
            category,
            field,
            label: label.into(),
            path,
            placeholder: "Same as Settings".into(),
            help: "A folder for this content type, or empty for the default".into(),
            choose_title: "Choose…".into(),
            choose_help: "Pick a folder".into(),
            clear_help: "Use the default".into(),
        })
        .collect()
}

/// The folder editor of `record`, with the different-disk `warnings` of its pairs (`Core::tracker_volume_warnings`).
#[uniffi::export]
pub fn tracker_folders_view(record: ProfileRecord, warnings: Vec<VolumeWarning>) -> TrackerFoldersView {
    let warned = |c: Option<Category>| warnings.iter().any(|w| w.category == c);
    TrackerFoldersView {
        rows: rows(&record, None),
        warning: warned(None).then(|| settings_different_disks(false)),
        note: "Torrents of this tracker use these folders instead of the ones in Settings → Folders; a rule's own Finished folder still comes first.".into(),
        types_title: "By content type".into(),
        types_open: !record.categories.is_empty(),
        types: TYPES
            .iter()
            .map(|&c| {
                let (i, f) = pair(&record, Some(c));
                TrackerFolderType {
                    category: c,
                    title: category_label(c),
                    custom: (i.is_some() || f.is_some()).then(|| "custom folders".into()),
                    warning: warned(Some(c)).then(|| settings_different_disks(true)),
                    rows: rows(&record, Some(c)),
                }
            })
            .collect(),
    }
}

/// `record` with one folder changed (`path` `None` or blank: Same as Settings). A content type left with no folder
/// of its own is no longer listed in the record.
#[uniffi::export]
pub fn tracker_folders_set(record: ProfileRecord, category: Option<Category>, field: TrackerFolderField, path: Option<String>) -> ProfileRecord {
    let mut r = record;
    let path = path.filter(|p| !p.trim().is_empty());
    match category {
        None => match field {
            TrackerFolderField::Incomplete => r.incomplete_dir = path,
            TrackerFolderField::Finished => r.finished_dir = path,
        },
        Some(c) => {
            let (mut incomplete, mut finished) = pair(&r, Some(c));
            match field {
                TrackerFolderField::Incomplete => incomplete = path,
                TrackerFolderField::Finished => finished = path,
            }
            let at = r.categories.iter().position(|s| s.category == c);
            let s = CategoryLocationSetting { category: c, incomplete_dir: incomplete, finished_dir: finished };
            match (at, s.incomplete_dir.is_some() || s.finished_dir.is_some()) {
                (Some(i), true) => r.categories[i] = s,
                (Some(i), false) => {
                    r.categories.remove(i);
                }
                (None, true) => r.categories.push(s),
                (None, false) => {}
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> ProfileRecord {
        ProfileRecord {
            id: "tl".into(),
            name: "TL".into(),
            urls: vec!["t.org".into()],
            rule: None,
            color: None,
            icon: None,
            down_limit: None,
            up_limit: None,
            incomplete_dir: None,
            finished_dir: None,
            categories: Vec::new(),
            hidden: false,
            hide_only_seeding: false,
            allow_pex: false,
        }
    }

    #[test]
    fn a_tracker_without_folders_says_same_as_settings_everywhere_and_keeps_its_types_closed() {
        let v = tracker_folders_view(rec(), Vec::new());
        let labels: Vec<_> = v.rows.iter().map(|r| (r.label.as_str(), r.path.clone(), r.placeholder.as_str())).collect();
        assert_eq!(labels, [("Incomplete", None, "Same as Settings"), ("Finished", None, "Same as Settings")]);
        assert!(v.note.starts_with("Torrents of this tracker use these folders"));
        assert_eq!((v.types_title.as_str(), v.types_open, v.warning.clone()), ("By content type", false, None));
        let titles: Vec<_> = v.types.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["Movie", "TV", "Music", "Audiobook", "Book", "App", "Game", "Other"]);
        assert!(v.types.iter().all(|t| t.custom.is_none() && t.warning.is_none() && t.rows.len() == 2));
        assert_eq!(v.types[0].rows[1].category, Some(Category::Movie));
    }

    #[test]
    fn a_folder_for_a_content_type_adds_its_entry_and_emptying_it_removes_the_entry() {
        let r = tracker_folders_set(rec(), Some(Category::Movie), TrackerFolderField::Finished, Some("/Volumes/M/TL/Movies".into()));
        assert_eq!(r.categories.len(), 1);
        assert_eq!(r.categories[0].finished_dir.as_deref(), Some("/Volumes/M/TL/Movies"));
        let v = tracker_folders_view(r.clone(), Vec::new());
        assert!(v.types_open, "a type with folders opens the list");
        assert_eq!(v.types[0].custom.as_deref(), Some("custom folders"));
        assert_eq!(v.types[0].rows[1].path.as_deref(), Some("/Volumes/M/TL/Movies"));
        assert!(v.types[1].custom.is_none(), "TV has none");
        let r = tracker_folders_set(r, Some(Category::Movie), TrackerFolderField::Incomplete, Some("/tmp/in".into()));
        assert_eq!((r.categories.len(), r.categories[0].incomplete_dir.as_deref()), (1, Some("/tmp/in")));
        let r = tracker_folders_set(r, Some(Category::Movie), TrackerFolderField::Incomplete, None);
        let r = tracker_folders_set(r, Some(Category::Movie), TrackerFolderField::Finished, Some("  ".into()));
        assert!(r.categories.is_empty(), "a blank folder is none, and a type with none goes");
    }

    #[test]
    fn the_trackers_own_pair_and_the_warnings() {
        let r = tracker_folders_set(rec(), None, TrackerFolderField::Incomplete, Some("/fast/in".into()));
        let r = tracker_folders_set(r, None, TrackerFolderField::Finished, Some("/slow/done".into()));
        assert_eq!((r.incomplete_dir.as_deref(), r.finished_dir.as_deref()), (Some("/fast/in"), Some("/slow/done")));
        assert!(r.categories.is_empty());
        let warnings = vec![
            VolumeWarning { category: None, incomplete: "/fast/in".into(), finished: "/slow/done".into() },
            VolumeWarning { category: Some(Category::Tv), incomplete: "/a".into(), finished: "/b".into() },
        ];
        let v = tracker_folders_view(r, warnings);
        assert_eq!(v.warning, Some(settings_different_disks(false)));
        assert_eq!(v.types[1].warning, Some(settings_different_disks(true)));
        assert!(v.types[0].warning.is_none());
        assert_eq!(v.rows[0].path.as_deref(), Some("/fast/in"));
    }
}
