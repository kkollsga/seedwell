//! The Folder Sync submenu of the menu bar item (SPEC 8.2 item 5, 7G.5) as a view model every host draws: the Mac's
//! NSMenu, the Windows tray flyout. Rust decides (docs/CROSS-PLATFORM.md §1) which rows there are and in what order,
//! each row's title, grey line, status image and accessibility words, the delete confirmation under a row and its
//! texts, the Add Folder… item, and when the submenu is rebuilt rather than updated (`signature`). The host draws,
//! tracks hover and the keyboard, and runs the commands.
//!
//! - [`SyncRemoval`]: the delete confirmation that is open (one at a time).
//! - [`sync_menu_model`]: the submenu.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use trs_control::display as d;
use trs_platform::computer;

use crate::sync::SyncSnapshot;
use crate::sync_text::{badge, subtitle, sync_remove_question, sync_remove_trash_label, SyncBadge, TITLE_LIMIT};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// MARK: The delete confirmation

#[derive(Default)]
struct RemovalState {
    /// The folder whose confirmation is open.
    open: Option<String>,
    /// "Also move the files to the Trash".
    trash: bool,
}

impl RemovalState {
    fn toggle(&mut self, id: &str) {
        if self.open.as_deref() == Some(id) {
            self.close();
        } else {
            self.open = Some(id.to_string());
            self.trash = false;
        }
    }

    fn close(&mut self) {
        self.open = None;
        self.trash = false;
    }

    fn keep_only(&mut self, ids: &HashSet<&str>) {
        if self.open.as_deref().is_some_and(|o| !ids.contains(o)) {
            self.close();
        }
    }
}

/// The delete confirmation under a row of the submenu (SPEC 8.2 item 5): one open at a time, each opening starts
/// without the Trash option, a folder removed elsewhere takes its question with it, and the host closes it whenever the
/// menu opens.
#[derive(uniffi::Object, Default)]
pub struct SyncRemoval {
    state: Mutex<RemovalState>,
}

#[uniffi::export]
impl SyncRemoval {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The trash button of a row: opens its confirmation (closing another one), or closes its own.
    pub fn toggle(&self, id: String) {
        lock(&self.state).toggle(&id);
    }

    /// Cancel, Remove, and every opening of the menu.
    pub fn close(&self) {
        lock(&self.state).close();
    }

    pub fn is_open(&self, id: String) -> bool {
        lock(&self.state).open.as_deref() == Some(id.as_str())
    }

    /// The folder whose confirmation is open.
    pub fn open_folder(&self) -> Option<String> {
        lock(&self.state).open.clone()
    }

    /// "Also move the files to the Trash" is ticked.
    pub fn trash(&self) -> bool {
        lock(&self.state).trash
    }

    pub fn set_trash(&self, on: bool) {
        lock(&self.state).trash = on;
    }
}

// MARK: The submenu

/// One synced folder in the submenu: status image, name and grey line; Edit and Remove at the right edge while the
/// pointer (or the keyboard) is on it. A click elsewhere reveals the folder in the file manager.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SyncMenuRow {
    /// The folder's id.
    pub id: String,
    /// The name, already cut in the middle where the menu cuts it.
    pub title: String,
    /// "Up to date · 3 devices", "Syncing · 42% · 12 MB/s".
    pub subtitle: String,
    pub badge: SyncBadge,
    /// What a screen reader says for the row: "Photos, Up to date".
    pub accessibility_label: String,
    /// The Edit and Remove buttons' names: "Edit Photos", "Remove Photos".
    pub edit_label: String,
    pub remove_label: String,
    /// Its confirmation is open under it: the trash button stays visible and lit.
    pub confirming: bool,
}

/// The confirmation under the row of `folder_id`: the question, the Trash option, then Cancel and Remove at the bottom
/// right. The menu stays open while it is used.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncMenuConfirm {
    pub folder_id: String,
    /// "Stop syncing “Photos” on this Mac? Other devices keep their copy."
    pub question: String,
    pub trash_label: String,
    /// The Trash option is ticked.
    pub trash: bool,
    pub cancel_title: String,
    pub cancel_help: String,
    pub remove_title: String,
    pub remove_help: String,
}

/// A plain item: its title and its tooltip.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncMenuItem {
    pub title: String,
    pub help: String,
}

/// The whole submenu, top to bottom: the folders (a confirmation under the one being removed), a separator, Add
/// Folder…. Two hidden items carry the keyboard's way to the hover buttons (Edit and Remove of the highlighted folder).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SyncMenuModel {
    pub rows: Vec<SyncMenuRow>,
    pub confirm: Option<SyncMenuConfirm>,
    /// "Add Folder…": the add modal.
    pub add: SyncMenuItem,
    /// "Edit Folder…" (shortcut E) and "Remove Folder…" (shortcut Delete), hidden: a key equivalent closes the menu, so
    /// Remove asks on the folder's page in the window instead.
    pub edit_title: String,
    pub remove_title: String,
    /// When it differs from the last one the submenu is rebuilt (a folder added, removed or renamed, a confirmation
    /// opened or closed); otherwise the rows' texts and images are updated in place (the highlight stays put).
    pub signature: String,
}

/// The submenu for `snapshot`, with the confirmation `removal` has open. A confirmation whose folder is gone closes.
#[uniffi::export]
pub fn sync_menu_model(snapshot: SyncSnapshot, removal: Arc<SyncRemoval>) -> SyncMenuModel {
    let mut r = lock(&removal.state);
    r.keep_only(&snapshot.folders.iter().map(|f| f.id.as_str()).collect());
    build(&snapshot, &r)
}

fn build(s: &SyncSnapshot, r: &RemovalState) -> SyncMenuModel {
    let open = r.open.as_deref();
    let rows = s
        .folders
        .iter()
        .map(|f| {
            let sub = subtitle(f);
            SyncMenuRow {
                id: f.id.clone(),
                title: d::middle_truncated(&f.name, TITLE_LIMIT),
                accessibility_label: format!("{}, {sub}", f.name),
                subtitle: sub,
                badge: badge(f),
                edit_label: format!("Edit {}", f.name),
                remove_label: format!("Remove {}", f.name),
                confirming: open == Some(f.id.as_str()),
            }
        })
        .collect();
    let confirm = open.and_then(|id| s.folders.iter().find(|f| f.id == id)).map(|f| SyncMenuConfirm {
        folder_id: f.id.clone(),
        question: sync_remove_question(f.name.clone()),
        trash_label: sync_remove_trash_label(),
        trash: r.trash,
        cancel_title: "Cancel".into(),
        cancel_help: "Keep syncing this folder".into(),
        remove_title: "Remove".into(),
        remove_help: concat!("Stop syncing this folder on this ", computer!()).into(),
    });
    let structure: Vec<String> = s.folders.iter().map(|f| format!("{}:{}", f.id, f.name)).collect();
    SyncMenuModel {
        rows,
        confirm,
        add: SyncMenuItem {
            title: "Add Folder…".into(),
            help: concat!("Sync a folder of this ", computer!(), ", or one shared with you").into(),
        },
        edit_title: "Edit Folder…".into(),
        remove_title: "Remove Folder…".into(),
        signature: format!("{}|{}", structure.join(","), open.unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::SyncState;
    use crate::sync_text::tests::folder;

    fn snap(folders: Vec<crate::sync::SyncFolder>) -> SyncSnapshot {
        SyncSnapshot {
            running: true,
            problem: None,
            device_id: "D".into(),
            device_name: "Mac".into(),
            port: 22100,
            folders,
            devices: Vec::new(),
        }
    }

    #[test]
    fn one_delete_confirmation_is_open_at_a_time() {
        let r = SyncRemoval::new();
        assert_eq!(r.open_folder(), None);
        r.toggle("A".into());
        assert!(r.is_open("A".into()));
        r.set_trash(true);
        r.toggle("B".into());
        assert!(r.is_open("B".into()) && !r.is_open("A".into()), "another row closes the first");
        assert!(!r.trash(), "each opening starts without the Trash");
        r.toggle("B".into());
        assert_eq!(r.open_folder(), None, "the trash button again closes it");
        r.toggle("C".into());
        lock(&r.state).keep_only(&["A", "B"].into_iter().collect());
        assert_eq!(r.open_folder(), None, "a folder removed elsewhere takes its question with it");
        r.toggle("A".into());
        r.set_trash(true);
        r.close(); // every opening of the menu
        assert_eq!((r.open_folder(), r.trash()), (None, false));
    }

    #[test]
    fn rows_carry_their_texts_and_images_in_the_folders_order() {
        let mut long = folder("Holiday photos from the summer house 2026 (all)", SyncState::Syncing);
        long.need_bytes = 580_000_000;
        long.rate_in = 12_000_000;
        let m = sync_menu_model(snap(vec![folder("Photos", SyncState::UpToDate), long]), SyncRemoval::new());
        assert_eq!(m.rows.len(), 2);
        let r = &m.rows[0];
        assert_eq!((r.id.as_str(), r.title.as_str(), r.subtitle.as_str()), ("ID-Photos", "Photos", "Up to date"));
        assert_eq!(r.badge, SyncBadge::UpToDate);
        assert_eq!(r.accessibility_label, "Photos, Up to date");
        assert_eq!((r.edit_label.as_str(), r.remove_label.as_str()), ("Edit Photos", "Remove Photos"));
        assert!(!r.confirming);
        let l = &m.rows[1];
        assert_eq!(l.title.chars().count(), TITLE_LIMIT, "a long name is cut in the middle");
        assert!(l.title.contains('…') && l.title.starts_with("Holiday") && l.title.ends_with("(all)"));
        assert!(l.accessibility_label.starts_with("Holiday photos from the summer house 2026 (all), Syncing · 42%"), "the whole name is read");
        assert_eq!(l.badge, SyncBadge::Progress { fraction: 0.42 });
        assert_eq!(crate::sync_text::sync_badge_label(l.badge), "syncing");
        assert_eq!(m.confirm, None);
        assert_eq!(m.add.title, "Add Folder…");
        assert_eq!(m.add.help, format!("Sync a folder of this {}, or one shared with you", computer!()));
        assert_eq!((m.edit_title.as_str(), m.remove_title.as_str()), ("Edit Folder…", "Remove Folder…"));
    }

    #[test]
    fn the_confirmation_goes_under_its_row_with_its_texts() {
        let removal = SyncRemoval::new();
        removal.toggle("ID-B".into());
        removal.set_trash(true);
        let m = sync_menu_model(snap(vec![folder("A", SyncState::UpToDate), folder("B", SyncState::UpToDate)]), removal.clone());
        assert_eq!(m.rows.iter().map(|r| r.confirming).collect::<Vec<_>>(), [false, true]);
        let c = m.confirm.unwrap();
        assert_eq!(c.folder_id, "ID-B");
        assert_eq!(c.question, format!("Stop syncing “B” on this {}? Other devices keep their copy.", computer!()));
        assert_eq!((c.trash_label.as_str(), c.trash), (concat!("Also move the files to the ", trs_platform::trash!()), true));
        assert_eq!((c.cancel_title.as_str(), c.cancel_help.as_str()), ("Cancel", "Keep syncing this folder"));
        assert_eq!((c.remove_title.as_str(), c.remove_help), ("Remove", format!("Stop syncing this folder on this {}", computer!())));
        // B is removed elsewhere: its question goes with it.
        let m = sync_menu_model(snap(vec![folder("A", SyncState::UpToDate)]), removal.clone());
        assert_eq!(m.confirm, None);
        assert_eq!(removal.open_folder(), None);
    }

    #[test]
    fn the_submenu_rebuilds_for_its_folders_and_its_confirmation_only() {
        let sig = |folders, removal: &Arc<SyncRemoval>| sync_menu_model(snap(folders), removal.clone()).signature;
        let r = SyncRemoval::new();
        let one = sig(vec![folder("A", SyncState::UpToDate)], &r);
        let mut busy = folder("A", SyncState::Syncing);
        busy.need_bytes = 5;
        assert_eq!(sig(vec![busy], &r), one, "a state change only updates the rows");
        assert_ne!(sig(vec![folder("A", SyncState::UpToDate), folder("B", SyncState::UpToDate)], &r), one, "a folder added");
        let renamed = crate::sync::SyncFolder { name: "A2".into(), ..folder("A", SyncState::UpToDate) };
        assert_ne!(sig(vec![renamed], &r), one, "a folder renamed");
        r.toggle("ID-A".into());
        assert_ne!(sig(vec![folder("A", SyncState::UpToDate)], &r), one, "a confirmation opened");
        r.close();
        assert_eq!(sig(vec![folder("A", SyncState::UpToDate)], &r), one);
        assert!(sync_menu_model(snap(Vec::new()), r).rows.is_empty());
    }
}
