//! The questions the app asks on its own (docs/CROSS-PLATFORM.md §1): adding a torrent a watch folder in "Ask first"
//! mode found, moving the finished torrents after their location changed (SPEC 7A.5), and adding the `.torrent` files
//! that were already in a watch folder (the backlog offer). The host shows each as an alert with these words and calls
//! what the answer says.

use crate::display::fmt_bytes;
use crate::row_text::category_label;
use crate::settings_text::{backlog_offer, BacklogOffer};
use crate::sync_text::SyncAlert;
use crate::types::{AddRequestInfo, MoveProposalInfo, MoveResult};
use crate::settings::WatchFolderSetting;
use crate::Core;

/// The marker in the state folder that says the backlog offer was made (it is made once).
const BACKLOG_MARKER: &str = "backlog-offered";

/// "Add “Night of the Living Dead (1968)”?" (a watch folder in "Ask first" mode, `on_add_request`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AddRequestPrompt {
    pub title: String,
    /// "Movie · 1.4 GB · tracker-y.net".
    pub text: String,
    /// "Add": the host adds [`AddRequestPrompt::path`] (`Core::add_source`).
    pub add_title: String,
    /// "Don't Add": nothing happens.
    pub dont_add_title: String,
    /// The `.torrent` file to add.
    pub path: String,
}

/// The question for an add request.
#[uniffi::export]
pub fn add_request_prompt(request: AddRequestInfo) -> AddRequestPrompt {
    let mut text = format!("{} · {}", category_label(request.category), fmt_bytes(request.total_bytes));
    if !request.tracker.is_empty() {
        text.push_str(&format!(" · {}", request.tracker));
    }
    AddRequestPrompt {
        title: format!("Add “{}”?", request.name),
        text,
        add_title: "Add".into(),
        dont_add_title: "Don't Add".into(),
        path: request.path,
    }
}

/// "Move the 3 existing torrents (12 GB) to the new location?" (SPEC 7A.5, after `on_locations_changed`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct LocationMovePrompt {
    pub title: String,
    pub text: String,
    /// "Move": the host calls `Core::move_torrents(ids)`, then shows [`location_move_result`]'s alert if there is one.
    pub move_title: String,
    /// "Don't Move": nothing moves.
    pub dont_move_title: String,
    pub ids: Vec<String>,
}

/// The question for the torrents `Core::location_moves` found elsewhere than the settings now say; `None` when there
/// are none (ask nothing). The host asks one at a time: an event while the question is open is dropped.
#[uniffi::export]
pub fn location_move_prompt(proposal: MoveProposalInfo) -> Option<LocationMovePrompt> {
    let n = proposal.items.len();
    if n == 0 {
        return None;
    }
    let size = if proposal.total_bytes > 0 { format!(" ({})", fmt_bytes(proposal.total_bytes)) } else { String::new() };
    let mut text = String::from("They keep seeding while they move.");
    if proposal.cross_volume > 0 {
        text.push_str(&format!(
            " {} on another disk, so their files are copied and checked first; the rest move instantly.",
            proposal.cross_volume
        ));
    } else {
        text.push_str(" Same disk: the files move instantly, nothing is copied.");
    }
    Some(LocationMovePrompt {
        title: format!("Move the {n} existing torrent{}{size} to the new location?", if n == 1 { "" } else { "s" }),
        text,
        move_title: "Move".into(),
        dont_move_title: "Don't Move".into(),
        ids: proposal.items.into_iter().map(|i| i.id).collect(),
    })
}

/// After Move: the alert for the torrents that were not moved (the first three reasons), or `None`.
#[uniffi::export]
pub fn location_move_result(result: MoveResult) -> Option<SyncAlert> {
    if result.refused.is_empty() {
        return None;
    }
    Some(SyncAlert { title: "Some torrents were not moved".into(), text: result.refused.into_iter().take(3).collect::<Vec<_>>().join("\n") })
}

/// The backlog offer with the files it adds.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BacklogPrompt {
    pub offer: BacklogOffer,
    /// What [`BacklogOffer::add_title`] adds, each with `Core::add_source`. "Not Now" adds nothing.
    pub files: Vec<String>,
}

/// The first enabled watch folder that holds a backlog, with its files.
pub(crate) fn backlog_pick(folders: &[WatchFolderSetting], backlog: impl Fn(&str) -> Vec<String>) -> Option<(&WatchFolderSetting, Vec<String>)> {
    folders.iter().filter(|w| w.enabled).find_map(|w| Some(backlog(&w.path)).filter(|f| !f.is_empty()).map(|f| (w, f)))
}

impl Core {
    /// Watch folders pick up only new files. The `.torrent` files that were already there when a folder joined the list
    /// are offered once ever, at start: "Add 5 existing torrents from Downloads?" (the first folder that has some).
    /// `None` when there is nothing to offer or it was offered before; the host asks at once.
    pub fn backlog_prompt(&self) -> Option<BacklogPrompt> {
        let marker = self.dir.join(BACKLOG_MARKER);
        if marker.exists() {
            return None;
        }
        let settings = self.settings_load();
        let pick = backlog_pick(&settings.folders.watch, |path| self.watch_backlog(path.to_owned()));
        let _ = std::fs::write(&marker, "offered");
        let (folder, files) = pick?;
        let count = u32::try_from(files.len()).unwrap_or(u32::MAX);
        Some(BacklogPrompt { offer: backlog_offer(count, folder.path.clone(), folder.after_add), files })
    }
}

#[uniffi::export]
impl Core {
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{TorrentFileAction, WatchModeSetting};
    use crate::types::{Category, MoveItemInfo};

    fn item(id: &str, cross_volume: bool) -> MoveItemInfo {
        MoveItemInfo { id: id.into(), name: id.into(), bytes: 4_000_000_000, from: "/a".into(), to: "/b".into(), cross_volume }
    }

    #[test]
    fn an_add_request_names_the_torrent_its_kind_size_and_tracker() {
        let r = AddRequestInfo {
            path: "/w/night.torrent".into(),
            name: "Night of the Living Dead (1968)".into(),
            total_bytes: 1_468_006_400,
            category: Category::Movie,
            tracker: "tracker-y.net".into(),
        };
        let p = add_request_prompt(r.clone());
        assert_eq!(p.title, "Add “Night of the Living Dead (1968)”?");
        assert_eq!(p.text, format!("Movie · {} · tracker-y.net", fmt_bytes(1_468_006_400)));
        assert_eq!((p.add_title.as_str(), p.dont_add_title.as_str(), p.path.as_str()), ("Add", "Don't Add", "/w/night.torrent"));
        // No tracker (a torrent without one): no empty part.
        let p = add_request_prompt(AddRequestInfo { tracker: String::new(), ..r });
        assert_eq!(p.text, format!("Movie · {}", fmt_bytes(1_468_006_400)));
    }

    #[test]
    fn the_location_move_counts_the_torrents_and_says_how_they_move() {
        assert_eq!(location_move_prompt(MoveProposalInfo { items: Vec::new(), total_bytes: 0, cross_volume: 0 }), None);
        let p = location_move_prompt(MoveProposalInfo { items: vec![item("a", true), item("b", false), item("c", false)], total_bytes: 12_000_000_000, cross_volume: 1 })
            .unwrap();
        assert_eq!(p.title, format!("Move the 3 existing torrents ({}) to the new location?", fmt_bytes(12_000_000_000)));
        assert_eq!(p.text, "They keep seeding while they move. 1 on another disk, so their files are copied and checked first; the rest move instantly.");
        assert_eq!((p.move_title.as_str(), p.dont_move_title.as_str()), ("Move", "Don't Move"));
        assert_eq!(p.ids, ["a", "b", "c"]);
        let one = location_move_prompt(MoveProposalInfo { items: vec![item("a", false)], total_bytes: 0, cross_volume: 0 }).unwrap();
        assert_eq!(one.title, "Move the 1 existing torrent to the new location?", "no size when nothing is known");
        assert_eq!(one.text, "They keep seeding while they move. Same disk: the files move instantly, nothing is copied.");
    }

    #[test]
    fn torrents_that_did_not_move_are_named_three_at_most() {
        assert_eq!(location_move_result(MoveResult { queued: 2, refused: Vec::new() }), None);
        let refused = ["a is checking", "b is gone", "c is moving", "d is busy"].map(String::from).to_vec();
        let a = location_move_result(MoveResult { queued: 0, refused }).unwrap();
        assert_eq!(a.title, "Some torrents were not moved");
        assert_eq!(a.text, "a is checking\nb is gone\nc is moving");
    }

    fn watch(path: &str, enabled: bool) -> WatchFolderSetting {
        WatchFolderSetting {
            path: path.into(),
            enabled,
            mode: WatchModeSetting::Auto,
            after_add: TorrentFileAction::Trash,
            archive_dir: None,
            category: None,
            rule: None,
            since: None,
        }
    }

    #[test]
    fn the_backlog_offer_takes_the_first_enabled_folder_with_files() {
        let folders = [watch("/off", false), watch("/empty", true), watch("/full", true), watch("/later", true)];
        let backlog = |p: &str| if p == "/empty" { Vec::new() } else { vec![format!("{p}/a.torrent")] };
        let (w, files) = backlog_pick(&folders, backlog).unwrap();
        assert_eq!((w.path.as_str(), files), ("/full", vec!["/full/a.torrent".to_string()]));
        assert!(backlog_pick(&folders[..2], backlog).is_none());
    }

    #[test]
    fn the_backlog_offer_is_made_once_ever() {
        let (core, dir) = crate::library::tests::mock();
        assert_eq!(core.backlog_prompt(), None, "the mock's folders hold nothing old");
        assert!(dir.path().join(BACKLOG_MARKER).exists(), "asked once: never again, also with nothing to offer");
        assert_eq!(core.backlog_prompt(), None);
    }
}
