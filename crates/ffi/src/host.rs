//! Which windows and dialogs a host has (docs/CROSS-PLATFORM.md §1). The command models offer only what the host can
//! open: a host that lacks a window never shows the commands that need it, instead of opening a placeholder. The Mac
//! has them all ([`host_windows_all`]); a host that grows a window adds it to the set it passes.

use crate::library_rows::RowDisclosure;
use crate::row_commands::{CommandEntry, RowCommand};
use crate::sidebar::{SidebarCommand, SidebarMenuEntry};

/// A window or dialog a command opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum HostWindow {
    /// A torrent's Get Info (`RowCommand::GetInfo`, the rows' info button).
    GetInfo,
    /// The chooser of what to download (`OpenChooser`, `ChooseFiles`).
    Chooser,
    /// A folder picker for Move to….
    MoveTo,
    /// The ratio dialog of Seeding Goal › Custom….
    CustomGoal,
    /// Settings' rules editor (Rule › Edit Rules…).
    RulesEditor,
    /// Starting a video player (Play, Play With, the rows' Play buttons).
    Player,
    /// A tracker's Info page (the sidebar's Info).
    TrackerInfo,
    /// A favorite tracker's rename in place and its Remove from Favorites question.
    FavoriteEditing,
    /// A channel's Share dialog.
    ChannelShare,
    /// The New Channel window (the sidebar's "New Channel…").
    NewChannel,
    /// The chat dialogs of a channel (`chat_dialogs`: New Topic…, Rename…, Leave Channel…).
    ChatDialogs,
    /// The file list under a torrent's row: in the window (the disclosure triangle, → and ←, Space on file rows) and the
    /// menu's inline checklist (a row's chevron, and the Play icon that comes with it).
    FileList,
    /// The Add Folder dialog of Folder Sync (`sync_add_window`).
    AddFolder,
    /// A synced folder's Edit dialog (name, place, options).
    FolderEditor,
    /// A synced folder's Share dialog (links, roles, the sharing period).
    FolderShare,
    /// The tracker log viewer (Settings › Advanced › Developer › View Log…).
    TrackerLog,
}

/// Every window: what the Mac passes.
#[uniffi::export]
pub fn host_windows_all() -> Vec<HostWindow> {
    vec![
        HostWindow::GetInfo,
        HostWindow::Chooser,
        HostWindow::MoveTo,
        HostWindow::CustomGoal,
        HostWindow::RulesEditor,
        HostWindow::Player,
        HostWindow::TrackerInfo,
        HostWindow::FavoriteEditing,
        HostWindow::ChannelShare,
        HostWindow::NewChannel,
        HostWindow::ChatDialogs,
        HostWindow::FileList,
        HostWindow::AddFolder,
        HostWindow::FolderEditor,
        HostWindow::FolderShare,
        HostWindow::TrackerLog,
    ]
}

/// Something a host can do that is not a window: the models leave out what needs one the host lacks, the same way
/// they leave out the commands of a missing [`HostWindow`]. The Mac has them all ([`host_features_all`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum HostFeature {
    /// Files on demand (SPEC 7G.11): folders the file manager lists whole and fetches file by file when opened (the
    /// Mac: a File Provider domain). Without it nothing offers or mentions on-demand folders: the Add Folder and Edit
    /// dialogs' switch, its help and note, the folder page's notice and its `AllowOnDemand`.
    OnDemandFolders,
    /// The file manager's extensions for sync badges and the Share menu, switched on in the system's settings (the
    /// Mac: Finder Sync and the Share extension): Settings › Folder Sync's Finder rows.
    FinderExtensions,
    /// Linking the command-line tool onto the PATH from Settings › Agents (Install Command-Line Tool…).
    CommandLineLink,
}

/// Every feature: what the Mac passes.
#[uniffi::export]
pub fn host_features_all() -> Vec<HostFeature> {
    vec![HostFeature::OnDemandFolders, HostFeature::FinderExtensions, HostFeature::CommandLineLink]
}

/// The window `command` opens, if it opens one (the rest are core calls or the host's own services: the file manager,
/// the clipboard, the Remove… question the library model draws).
pub(crate) fn row_command_window(command: &RowCommand) -> Option<HostWindow> {
    match command {
        RowCommand::GetInfo => Some(HostWindow::GetInfo),
        RowCommand::OpenChooser | RowCommand::ChooseFiles => Some(HostWindow::Chooser),
        RowCommand::MoveTo => Some(HostWindow::MoveTo),
        RowCommand::CustomGoal => Some(HostWindow::CustomGoal),
        RowCommand::EditRules => Some(HostWindow::RulesEditor),
        RowCommand::Play { .. } | RowCommand::PlayRow => Some(HostWindow::Player),
        _ => None,
    }
}

/// The list a row's disclosure triangle opens under it.
pub(crate) fn disclosure_window(disclosure: RowDisclosure) -> HostWindow {
    match disclosure {
        RowDisclosure::Files => HostWindow::FileList,
        RowDisclosure::Choice => HostWindow::Chooser,
    }
}

/// The window a sidebar command opens, if it opens one.
pub(crate) fn sidebar_command_window(command: SidebarCommand) -> Option<HostWindow> {
    match command {
        SidebarCommand::TrackerInfo => Some(HostWindow::TrackerInfo),
        SidebarCommand::RenameTracker | SidebarCommand::RemoveFavorite => Some(HostWindow::FavoriteEditing),
        SidebarCommand::ShareChannel => Some(HostWindow::ChannelShare),
        SidebarCommand::NewTopic | SidebarCommand::RenameChannel | SidebarCommand::LeaveChannel => Some(HostWindow::ChatDialogs),
        SidebarCommand::Mute | SidebarCommand::Unmute | SidebarCommand::ChannelAgents | SidebarCommand::SaveAsFavorite | SidebarCommand::SetFollow { .. } => {
            None
        }
    }
}

/// `host` can open what `window` names (nothing needed is always there).
pub(crate) fn has(host: &[HostWindow], window: Option<HostWindow>) -> bool {
    window.is_none_or(|w| host.contains(&w))
}

/// A row command list without the commands `host` cannot open: a submenu left empty goes, and so does a separator
/// left at either end or next to another.
pub(crate) fn offered_commands(entries: Vec<CommandEntry>, host: &[HostWindow]) -> Vec<CommandEntry> {
    let mut out: Vec<CommandEntry> = Vec::with_capacity(entries.len());
    for e in entries {
        match e {
            CommandEntry::Item { ref command, .. } if !has(host, row_command_window(command)) => {}
            CommandEntry::Submenu { title, entries } => {
                let entries = offered_commands(entries, host);
                if entries.iter().any(|e| !matches!(e, CommandEntry::Separator)) {
                    out.push(CommandEntry::Submenu { title, entries });
                }
            }
            CommandEntry::Separator if matches!(out.last(), None | Some(CommandEntry::Separator)) => {}
            e => out.push(e),
        }
    }
    if matches!(out.last(), Some(CommandEntry::Separator)) {
        out.pop();
    }
    out
}

/// A sidebar menu without the commands `host` cannot open, its separators tidied.
pub(crate) fn offered_sidebar(entries: Vec<SidebarMenuEntry>, host: &[HostWindow]) -> Vec<SidebarMenuEntry> {
    let mut out: Vec<SidebarMenuEntry> = Vec::with_capacity(entries.len());
    for e in entries {
        match e {
            SidebarMenuEntry::Item { command, .. } if !has(host, sidebar_command_window(command)) => {}
            SidebarMenuEntry::Separator if matches!(out.last(), None | Some(SidebarMenuEntry::Separator)) => {}
            e => out.push(e),
        }
    }
    if matches!(out.last(), Some(SidebarMenuEntry::Separator)) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(command: RowCommand) -> CommandEntry {
        CommandEntry::Item { command, title: String::new(), checked: false, enabled: true, destructive: false, shortcut: None }
    }

    #[test]
    fn every_window_is_in_the_macs_set_once() {
        let all = host_windows_all();
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len());
        assert_eq!(all.len(), 16, "a new HostWindow belongs in host_windows_all");
        assert_eq!(host_features_all().len(), 3, "a new HostFeature belongs in host_features_all");
    }

    #[test]
    fn a_command_whose_window_is_missing_goes_with_its_empty_submenu_and_extra_separators() {
        let entries = vec![
            item(RowCommand::Pause),
            CommandEntry::Separator,
            item(RowCommand::GetInfo),
            item(RowCommand::MoveTo),
            CommandEntry::Separator,
            CommandEntry::Submenu { title: "Rule".into(), entries: vec![item(RowCommand::SetRule { rule_id: None }), CommandEntry::Separator, item(RowCommand::EditRules)] },
            CommandEntry::Submenu { title: "Play".into(), entries: vec![item(RowCommand::Play { file_index: Some(0), player_id: None })] },
            CommandEntry::Separator,
            item(RowCommand::Remove),
        ];
        assert_eq!(offered_commands(entries.clone(), &host_windows_all()), entries, "the Mac keeps everything");
        let got = offered_commands(entries, &[]);
        assert_eq!(
            got,
            vec![
                item(RowCommand::Pause),
                CommandEntry::Separator,
                CommandEntry::Submenu { title: "Rule".into(), entries: vec![item(RowCommand::SetRule { rule_id: None })] },
                CommandEntry::Separator,
                item(RowCommand::Remove),
            ]
        );
    }

    #[test]
    fn a_sidebar_menu_keeps_what_needs_no_window() {
        let e = |title: &str, command| SidebarMenuEntry::Item { title: title.into(), command, destructive: false, checked: false, enabled: true, help: String::new() };
        let menu = vec![e("Share…", SidebarCommand::ShareChannel), e("Mute", SidebarCommand::Mute), e("Rename…", SidebarCommand::RenameChannel), SidebarMenuEntry::Separator, e("Leave Channel…", SidebarCommand::LeaveChannel)];
        assert_eq!(offered_sidebar(menu.clone(), &host_windows_all()), menu);
        assert_eq!(offered_sidebar(menu, &[HostWindow::ChannelShare]), vec![e("Share…", SidebarCommand::ShareChannel), e("Mute", SidebarCommand::Mute)]);
    }
}
