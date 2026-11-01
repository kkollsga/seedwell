//! The app's own commands (SPEC §8.2): adding a torrent file or a link, Pause All, Resume All and Limit Speed, as one
//! small model every host draws: the Mac's menu bar menus (File, Transfers) and their key equivalents, the Windows
//! tray menu. Rust decides (docs/CROSS-PLATFORM.md §1) the titles, their order and grouping, the shortcuts, what is
//! enabled and the check mark; the host draws them and runs each [`AppCommand`] (a core call, or its own window).

use trs_platform::shortcut;

use crate::types::{Snapshot, TransferState};
use crate::Core;

/// What an app command does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum AppCommand {
    /// Asks for `.torrent` files (the system's Open dialog) and adds them (`add_source`).
    AddTorrentFile,
    /// Opens the library window with its "Add a Download" sheet (`Library::open_add_sheet`).
    AddLink,
    /// `Core::pause_all`.
    PauseAll,
    /// `Core::resume_all`.
    ResumeAll,
    /// `Core::set_alt_speed(on)`: Limit Speed switched to `on`.
    SetLimitSpeed { on: bool },
}

/// A key pressed with the system's command key (⌘ on the Mac, Ctrl on Windows and Linux).
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct KeyShortcut {
    /// The key as typed without Shift: a lowercase letter or a sign ("o", "l", ",").
    pub key: String,
    /// With Shift.
    pub shift: bool,
    /// With Option (Mac) or Alt.
    pub alt: bool,
    /// The whole shortcut as this system writes it, for a menu that draws it as text: "⇧⌘O", "Ctrl+Shift+O".
    pub label: String,
}

/// One line of an app menu.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum AppMenuEntry {
    Item {
        command: AppCommand,
        title: String,
        shortcut: Option<KeyShortcut>,
        enabled: bool,
        /// Drawn with a check mark (Limit Speed while it is on).
        checked: bool,
    },
    Separator,
}

/// A menu of app commands: "File", "Transfers". A host with one menu (the Windows tray) draws them one after the
/// other with a separator between.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct AppMenu {
    pub title: String,
    pub entries: Vec<AppMenuEntry>,
}

fn key(key: &str, shift: bool, alt: bool, label: &str) -> Option<KeyShortcut> {
    Some(KeyShortcut { key: key.into(), shift, alt, label: label.into() })
}

fn item(command: AppCommand, title: &str, shortcut: Option<KeyShortcut>, enabled: bool) -> AppMenuEntry {
    AppMenuEntry::Item { command, title: title.into(), shortcut, enabled, checked: false }
}

/// The app commands for the transfers in `s`. Pause All is enabled while something runs that it can pause, Resume
/// All while something is paused or stopped; with no transfer at all both are off.
pub(crate) fn app_menus(s: &Snapshot) -> Vec<AppMenu> {
    let downloads = || s.downloading.iter().chain(&s.queued).chain(&s.hidden_downloading).chain(&s.hidden_queued);
    let paused_downloads = downloads().filter(|r| r.state == TransferState::Paused).count();
    let running_downloads = downloads().count() - paused_downloads;
    let stopped_seeds = s.sharing.stopped;
    let running_seeds = s.sharing_total.saturating_sub(stopped_seeds);
    let can_pause = running_downloads > 0 || running_seeds > 0;
    let can_resume = paused_downloads > 0 || stopped_seeds > 0;
    vec![
        AppMenu {
            title: "File".into(),
            entries: vec![
                item(AppCommand::AddTorrentFile, "Add Torrent File…", key("o", false, false, shortcut!("O")), true),
                item(AppCommand::AddLink, "Add Magnet Link or URL…", key("o", true, false, shortcut!(shift "O")), true),
            ],
        },
        AppMenu {
            title: "Transfers".into(),
            entries: vec![
                item(AppCommand::PauseAll, "Pause All", None, can_pause),
                item(AppCommand::ResumeAll, "Resume All", None, can_resume),
                AppMenuEntry::Separator,
                AppMenuEntry::Item {
                    command: AppCommand::SetLimitSpeed { on: !s.alt_speed },
                    title: "Limit Speed".into(),
                    shortcut: key("l", false, true, shortcut!(alt "L")),
                    enabled: true,
                    checked: s.alt_speed,
                },
            ],
        },
    ]
}

#[uniffi::export]
impl Core {
    /// The app's own commands as they stand now (reads the snapshot): File's Add Torrent File… and Add Magnet Link or
    /// URL…, Transfers' Pause All, Resume All and Limit Speed. Ask again each time a menu opens.
    pub fn app_commands(&self) -> Vec<AppMenu> {
        app_menus(&self.backend.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_rows::tests::{download, seed};
    use crate::library::tests::snap;
    use crate::types::{SeedGroup, TransferRow};

    fn items(menus: &[AppMenu]) -> Vec<(String, AppCommand, Option<String>, bool, bool)> {
        menus
            .iter()
            .flat_map(|m| &m.entries)
            .filter_map(|e| match e {
                AppMenuEntry::Item { command, title, shortcut, enabled, checked } => {
                    Some((title.clone(), *command, shortcut.as_ref().map(|k| k.label.clone()), *enabled, *checked))
                }
                AppMenuEntry::Separator => None,
            })
            .collect()
    }

    fn find(menus: &[AppMenu], title: &str) -> (AppCommand, bool, bool) {
        let (_, c, _, e, k) = items(menus).into_iter().find(|i| i.0 == title).unwrap_or_else(|| panic!("no {title}"));
        (c, e, k)
    }

    #[test]
    fn the_menus_are_file_and_transfers_with_their_titles_and_shortcuts() {
        let m = app_menus(&snap());
        assert_eq!(m.iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), ["File", "Transfers"]);
        assert_eq!(m[1].entries[2], AppMenuEntry::Separator, "Limit Speed apart from Pause All and Resume All");
        let titles: Vec<String> = items(&m).into_iter().map(|i| i.0).collect();
        assert_eq!(titles, ["Add Torrent File…", "Add Magnet Link or URL…", "Pause All", "Resume All", "Limit Speed"]);
        let AppMenuEntry::Item { shortcut: Some(open), .. } = &m[0].entries[0] else { panic!() };
        assert_eq!((open.key.as_str(), open.shift, open.alt), ("o", false, false));
        let AppMenuEntry::Item { shortcut: Some(link), .. } = &m[0].entries[1] else { panic!() };
        assert_eq!((link.key.as_str(), link.shift, link.alt), ("o", true, false));
        let AppMenuEntry::Item { shortcut: Some(limit), .. } = &m[1].entries[3] else { panic!() };
        assert_eq!((limit.key.as_str(), limit.shift, limit.alt), ("l", false, true));
        #[cfg(target_os = "macos")]
        assert_eq!((open.label.as_str(), link.label.as_str(), limit.label.as_str()), ("⌘O", "⇧⌘O", "⌥⌘L"));
        #[cfg(not(target_os = "macos"))]
        assert_eq!((open.label.as_str(), link.label.as_str(), limit.label.as_str()), ("Ctrl+O", "Ctrl+Shift+O", "Ctrl+Alt+L"));
    }

    #[test]
    fn limit_speed_is_checked_while_on_and_switches_it_the_other_way() {
        let mut s = snap();
        assert_eq!(find(&app_menus(&s), "Limit Speed"), (AppCommand::SetLimitSpeed { on: true }, true, false));
        s.alt_speed = true;
        assert_eq!(find(&app_menus(&s), "Limit Speed"), (AppCommand::SetLimitSpeed { on: false }, true, true));
    }

    #[test]
    fn pause_all_and_resume_all_are_enabled_when_they_would_do_something() {
        let mut s = snap();
        let state = |s: &Snapshot| (find(&app_menus(s), "Pause All").1, find(&app_menus(s), "Resume All").1);
        assert_eq!(state(&s), (false, false), "nothing at all");
        assert!(find(&app_menus(&s), "Add Torrent File…").1 && find(&app_menus(&s), "Add Magnet Link or URL…").1);
        s.downloading = vec![download(TransferState::Downloading)];
        assert_eq!(state(&s), (true, false));
        s.queued = vec![download(TransferState::Paused)];
        assert_eq!(state(&s), (true, true));
        s.downloading.clear();
        assert_eq!(state(&s), (false, true), "only a paused download");
        s.queued.clear();
        s.hidden_queued = vec![download(TransferState::Queued)];
        assert_eq!(state(&s), (true, false), "hidden mode's downloads count too");
        let mut s = snap();
        s.sharing_busiest = vec![TransferRow { seed_group: Some(SeedGroup::Active), ..seed() }];
        s.sharing_total = 3;
        assert_eq!(state(&s), (true, false), "seeding");
        s.sharing.stopped = 3;
        assert_eq!(state(&s), (false, true), "every seed stopped");
        s.sharing.stopped = 1;
        assert_eq!(state(&s), (true, true));
    }
}
