//! The words of the Settings window's Folders tab and of the watch folders (SPEC §8.4, concept 7) that name the Trash:
//! what happens to a `.torrent` file after it was added, a watch folder's line, and the offer of the files that were
//! already in a watch folder. Rust words them (docs/CROSS-PLATFORM.md §1), with the system's name for the Trash
//! (`trs_platform::trash!`: the Recycle Bin on Windows); the host draws them.

use std::path::Path;

use trs_platform::trash;

use crate::menu::APP_NAME;
use crate::row_text::category_label;
use crate::settings::{TorrentFileAction, WatchFolderSetting, WatchModeSetting};

/// A choice of "After adding a .torrent ▸ The .torrent file": "Move to Trash", "Move to an archive folder", "Leave it".
#[uniffi::export]
pub fn torrent_file_action_label(action: TorrentFileAction) -> String {
    match action {
        TorrentFileAction::Trash => concat!("Move to ", trash!()),
        TorrentFileAction::Archive => "Move to an archive folder",
        TorrentFileAction::Leave => "Leave it",
    }
    .into()
}

/// A choice of a watch folder's "Then" menu: "Move .torrent to Trash", "Archive .torrent", "Leave .torrent".
#[uniffi::export]
pub fn watch_then_label(action: TorrentFileAction) -> String {
    match action {
        TorrentFileAction::Trash => concat!("Move .torrent to ", trash!()),
        TorrentFileAction::Archive => "Archive .torrent",
        TorrentFileAction::Leave => "Leave .torrent",
    }
    .into()
}

/// A watch folder's grey line: "Always treat as TV · Add automatically · then move .torrent to Trash".
#[uniffi::export]
pub fn watch_folder_detail(folder: WatchFolderSetting) -> String {
    let mode = match folder.mode {
        WatchModeSetting::Auto => "Add automatically",
        WatchModeSetting::Ask => "Ask first",
    };
    let then = match folder.after_add {
        TorrentFileAction::Trash => concat!("then move .torrent to ", trash!()),
        TorrentFileAction::Archive => "then archive .torrent",
        TorrentFileAction::Leave => "keep .torrent",
    };
    let line = format!("{mode} · {then}");
    match folder.category {
        Some(c) => format!("Always treat as {} · {line}", category_label(c)),
        None => line,
    }
}

/// The offer of the `.torrent` files that were already in a watch folder when it joined the list (they are not picked
/// up by themselves), with every text it shows.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BacklogOffer {
    /// "Add 5 existing torrents from Downloads?"
    pub title: String,
    /// Why it asks, and what happens to the files once added.
    pub text: String,
    /// "Add 5": adds them.
    pub add_title: String,
    /// "Not Now": the offer is not made again.
    pub later_title: String,
}

/// The offer of `count` files found in the watch folder at `folder_path`, which does `after_add` with them.
#[uniffi::export]
pub fn backlog_offer(count: u32, folder_path: String, after_add: TorrentFileAction) -> BacklogOffer {
    let folder = Path::new(&folder_path).file_name().map_or_else(|| folder_path.clone(), |n| n.to_string_lossy().into_owned());
    let then = match after_add {
        TorrentFileAction::Trash => concat!(" Added ones are moved to the ", trash!(), "."),
        TorrentFileAction::Archive => " Added ones are moved to the archive folder.",
        TorrentFileAction::Leave => " The .torrent files stay where they are.",
    };
    BacklogOffer {
        title: format!("Add {count} existing torrent{} from {folder}?", if count == 1 { "" } else { "s" }),
        text: format!("{APP_NAME} only picks up new files from a watched folder. These were already there.{then}"),
        add_title: format!("Add {count}"),
        later_title: "Not Now".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Category;

    fn watch(mode: WatchModeSetting, after_add: TorrentFileAction, category: Option<Category>) -> WatchFolderSetting {
        WatchFolderSetting { path: "/Users/me/Downloads".into(), enabled: true, mode, after_add, archive_dir: None, category, rule: None, since: None }
    }

    #[test]
    fn what_happens_to_a_torrent_file_names_the_systems_trash() {
        assert_eq!(torrent_file_action_label(TorrentFileAction::Trash), concat!("Move to ", trash!()));
        assert_eq!(torrent_file_action_label(TorrentFileAction::Archive), "Move to an archive folder");
        assert_eq!(torrent_file_action_label(TorrentFileAction::Leave), "Leave it");
        assert_eq!(watch_then_label(TorrentFileAction::Trash), concat!("Move .torrent to ", trash!()));
        assert_eq!(watch_then_label(TorrentFileAction::Leave), "Leave .torrent");
    }

    #[test]
    fn a_watch_folders_line_says_how_it_adds_and_what_follows() {
        assert_eq!(watch_folder_detail(watch(WatchModeSetting::Auto, TorrentFileAction::Trash, None)), concat!("Add automatically · then move .torrent to ", trash!()));
        assert_eq!(watch_folder_detail(watch(WatchModeSetting::Ask, TorrentFileAction::Archive, None)), "Ask first · then archive .torrent");
        assert_eq!(watch_folder_detail(watch(WatchModeSetting::Ask, TorrentFileAction::Leave, Some(Category::Tv))), "Always treat as TV · Ask first · keep .torrent");
    }

    #[test]
    fn the_backlog_offer_counts_the_files_and_says_where_they_go() {
        let o = backlog_offer(5, "/Users/me/Downloads".into(), TorrentFileAction::Trash);
        assert_eq!(o.title, "Add 5 existing torrents from Downloads?");
        assert_eq!(o.text, concat!("Seedwell only picks up new files from a watched folder. These were already there. Added ones are moved to the ", trash!(), "."));
        assert_eq!((o.add_title.as_str(), o.later_title.as_str()), ("Add 5", "Not Now"));
        let o = backlog_offer(1, "/Users/me/Torrents/".into(), TorrentFileAction::Leave);
        assert_eq!(o.title, "Add 1 existing torrent from Torrents?");
        assert!(o.text.ends_with("These were already there. The .torrent files stay where they are."));
        assert!(backlog_offer(2, "/".into(), TorrentFileAction::Archive).text.ends_with(" Added ones are moved to the archive folder."));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_mac_words_are_the_ones_it_always_had() {
        assert_eq!(torrent_file_action_label(TorrentFileAction::Trash), "Move to Trash");
        assert_eq!(watch_then_label(TorrentFileAction::Trash), "Move .torrent to Trash");
        assert_eq!(watch_folder_detail(watch(WatchModeSetting::Auto, TorrentFileAction::Trash, None)), "Add automatically · then move .torrent to Trash");
        let o = backlog_offer(5, "/Users/me/Downloads".into(), TorrentFileAction::Trash);
        assert_eq!(o.text, "Seedwell only picks up new files from a watched folder. These were already there. Added ones are moved to the Trash.");
    }
}
