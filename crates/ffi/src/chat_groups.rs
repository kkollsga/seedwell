//! Groups of channels in the sidebar (SPEC §7G.7 *Groups*): a name and an icon, in the user's order, that channels
//! are listed under. They are this device's own, like Mute (kept by folder sync's configuration, never sent to another
//! device). Rust decides (docs/CROSS-PLATFORM.md §1) the icons and their spoken names, what a new group's icon is when
//! none was picked, the dialogs' words and what the sidebar and the menus show (`sidebar.rs`, `chat_page.rs`); each
//! host maps an icon to its own symbol in one table (the Mac: `SidebarSymbols.group`, SF Symbols; Windows: Segoe
//! Fluent glyphs).

use crate::chat_dialogs::{ChatDialog, ChatDialogField, ChatDialogKind};
use crate::types::CoreError;
use crate::Core;

/// What a group's icon shows. The host picks the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ChatGroupIcon {
    /// Agents.
    Sparkles,
    Folder,
    /// Work.
    Briefcase,
    /// Home.
    House,
    /// Family, friends.
    People,
    Star,
    /// Development.
    Hammer,
    Book,
    GameController,
    Heart,
    Globe,
    Tray,
}

impl ChatGroupIcon {
    /// Every icon, in the order the Icon menu lists them.
    pub(crate) const ALL: [ChatGroupIcon; 12] = [
        ChatGroupIcon::Sparkles,
        ChatGroupIcon::Folder,
        ChatGroupIcon::Briefcase,
        ChatGroupIcon::House,
        ChatGroupIcon::People,
        ChatGroupIcon::Star,
        ChatGroupIcon::Hammer,
        ChatGroupIcon::Book,
        ChatGroupIcon::GameController,
        ChatGroupIcon::Heart,
        ChatGroupIcon::Globe,
        ChatGroupIcon::Tray,
    ];

    /// Its name in folder sync's configuration and the agent interface (`seedwell chat group new … --icon <name>`).
    pub(crate) fn name(self) -> &'static str {
        match self {
            ChatGroupIcon::Sparkles => "sparkles",
            ChatGroupIcon::Folder => "folder",
            ChatGroupIcon::Briefcase => "briefcase",
            ChatGroupIcon::House => "house",
            ChatGroupIcon::People => "people",
            ChatGroupIcon::Star => "star",
            ChatGroupIcon::Hammer => "hammer",
            ChatGroupIcon::Book => "book",
            ChatGroupIcon::GameController => "gamecontroller",
            ChatGroupIcon::Heart => "heart",
            ChatGroupIcon::Globe => "globe",
            ChatGroupIcon::Tray => "tray",
        }
    }

    /// The icon of a name; one this version does not know is a folder.
    pub(crate) fn from_name(name: &str) -> ChatGroupIcon {
        Self::ALL.into_iter().find(|i| i.name() == name).unwrap_or(ChatGroupIcon::Folder)
    }

    /// What it is called: the Icon menu's titles and what a screen reader says.
    pub(crate) fn label(self) -> &'static str {
        match self {
            ChatGroupIcon::Sparkles => "Sparkles",
            ChatGroupIcon::Folder => "Folder",
            ChatGroupIcon::Briefcase => "Briefcase",
            ChatGroupIcon::House => "House",
            ChatGroupIcon::People => "People",
            ChatGroupIcon::Star => "Star",
            ChatGroupIcon::Hammer => "Hammer",
            ChatGroupIcon::Book => "Book",
            ChatGroupIcon::GameController => "Game Controller",
            ChatGroupIcon::Heart => "Heart",
            ChatGroupIcon::Globe => "Globe",
            ChatGroupIcon::Tray => "Tray",
        }
    }
}

/// The spoken name of a group's icon ("Sparkles").
#[uniffi::export]
pub fn chat_group_icon_label(icon: ChatGroupIcon) -> String {
    icon.label().into()
}

/// The alert's title when a group command without a dialog fails (its text is the error's sentence): moving a channel
/// into or out of a group (`moving_channel`), or changing a group (its icon, its place, deleting it).
#[uniffi::export]
pub fn chat_group_failure_title(moving_channel: bool) -> String {
    if moving_channel { "Couldn't move the channel" } else { "Couldn't change the group" }.into()
}

/// A group of channels in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct SyncChatGroup {
    pub id: String,
    pub name: String,
    pub icon: ChatGroupIcon,
}

/// The icon a new group gets from its name when none was picked: "Agents" sparkles, "Work" a briefcase, "Family" people;
/// else a folder. The user changes it with the group's Icon menu.
/// Folder sync's rule (`icon_for_name`), so a group made in the app and one made by an agent get the same.
pub(crate) fn guess_icon(name: &str) -> ChatGroupIcon {
    ChatGroupIcon::from_name(seedwell_sync_net::chat_groups::icon_for_name(name))
}

/// The longest group name kept, in characters (as folder sync keeps it).
const GROUP_NAME_MAX: usize = seedwell_sync_net::chat_groups::GROUP_NAME_MAX;

fn group_dialog(kind: ChatDialogKind, title: String, message: &str, text: &str, confirm: &str, failure: &str) -> ChatDialog {
    ChatDialog {
        kind,
        title,
        message: message.into(),
        field: Some(ChatDialogField { label: "Name".into(), placeholder: "Agents, Work, Family…".into(), text: text.into() }),
        cancel_title: "Cancel".into(),
        confirm_title: confirm.into(),
        destructive: false,
        failure_title: failure.into(),
    }
}

/// New Group…: asks for the group's name. Its icon follows from the name (`Agents`: sparkles), else a folder; the
/// group's Icon menu changes it. From a channel's menu the channel goes into the new group.
#[uniffi::export]
pub fn chat_new_group_dialog() -> ChatDialog {
    group_dialog(
        ChatDialogKind::NewGroup,
        "New Group".into(),
        concat!("A group gathers channels under one name in the sidebar, on this ", seedwell_platform::computer!(), " only. Change its icon with a right-click on it."),
        "",
        "Start",
        "Couldn't make the group",
    )
}

/// Rename Group…: asks for the group's new name.
#[uniffi::export]
pub fn chat_rename_group_dialog(group: SyncChatGroup) -> ChatDialog {
    group_dialog(
        ChatDialogKind::RenameGroup,
        format!("Rename “{}”", group.name),
        concat!("The group is this ", seedwell_platform::computer!(), "'s own: its channels and the other devices are not changed."),
        &group.name,
        "Rename",
        "Couldn't rename the group",
    )
}

/// What the action of a group dialog does with `input`: the name trimmed and cut to its length, never empty.
pub(crate) fn check_group_name(dialog: &ChatDialog, input: &str) -> crate::chat_dialogs::ChatDialogInput {
    use crate::chat_dialogs::ChatDialogInput;
    let name: String = input.trim().chars().take(GROUP_NAME_MAX).collect();
    if name.is_empty() {
        return ChatDialogInput::Problem { text: "Give the group a name.".into() };
    }
    if dialog.kind == ChatDialogKind::RenameGroup && dialog.field.as_ref().is_some_and(|f| f.text.trim() == name) {
        return ChatDialogInput::Unchanged;
    }
    ChatDialogInput::Ready { value: Some(name) }
}

impl Core {
    /// Makes a group, last in the order, with `icon` (`None`: the one its name suggests, else a folder); `channel_id`:
    /// a channel that goes into it at once (New Group… of a channel's menu). The channel's move failing leaves the group.
    pub fn chat_group_new(&self, name: String, icon: Option<ChatGroupIcon>, channel_id: Option<String>) -> Result<SyncChatGroup, CoreError> {
        let sync = self.backend.sync();
        let icon = icon.unwrap_or_else(|| guess_icon(&name));
        let g = sync.chat_group_new(name.trim(), icon)?;
        if let Some(c) = channel_id {
            sync.chat_set_group(&c, Some(&g.id))?;
        }
        Ok(g)
    }

    /// Renames a group.
    pub fn chat_group_rename(&self, id: String, name: String) -> Result<(), CoreError> {
        self.backend.sync().chat_group_edit(&id, Some(name.trim()), None)
    }

    /// Changes a group's icon.
    pub fn chat_group_set_icon(&self, id: String, icon: ChatGroupIcon) -> Result<(), CoreError> {
        self.backend.sync().chat_group_edit(&id, None, Some(icon))
    }

    /// Moves a group one place up (`up`) or down in the order; at either end it stays.
    pub fn chat_group_move(&self, id: String, up: bool) -> Result<(), CoreError> {
        let sync = self.backend.sync();
        let groups = sync.chat_groups();
        let at = groups.iter().position(|g| g.id == id).ok_or_else(|| CoreError::Failed { message: "This group is no longer here.".into() })?;
        let to = if up { at.saturating_sub(1) } else { (at + 1).min(groups.len() - 1) };
        if to == at {
            return Ok(());
        }
        sync.chat_group_move(&id, to as u32)
    }

    /// Deletes a group: its channels go back to the plain list, as they are.
    pub fn chat_group_delete(&self, id: String) -> Result<(), CoreError> {
        self.backend.sync().chat_group_delete(&id)
    }

    /// Lists a channel under a group (`None`: under none).
    pub fn chat_set_group(&self, channel_id: String, group_id: Option<String>) -> Result<(), CoreError> {
        self.backend.sync().chat_set_group(&channel_id, group_id.as_deref())
    }
}

#[uniffi::export]
impl Core {
    /// The sidebar's groups of channels, in the user's order. Cheap (no disk, no network).
    pub fn chat_groups(&self) -> Vec<SyncChatGroup> {
        self.backend.sync().chat_groups()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches the hosts' icon names drifting from folder sync's and the CLI's: an icon saved by one would come back as
    /// another (or as a folder) in the other.
    #[test]
    fn every_icon_has_the_name_folder_sync_keeps_and_back() {
        let names: Vec<&str> = ChatGroupIcon::ALL.iter().map(|i| i.name()).collect();
        assert_eq!(names, seedwell_sync_net::chat_groups::GROUP_ICONS);
        for i in ChatGroupIcon::ALL {
            assert_eq!(ChatGroupIcon::from_name(i.name()), i);
            assert!(!i.label().is_empty());
        }
        assert_eq!(ChatGroupIcon::from_name("rocket"), ChatGroupIcon::Folder, "a newer version's icon: a folder");
    }

    /// Catches the user's own example getting a folder: a group named for agents gets sparkles; a word that only starts
    /// like a short key ("aid" is not "ai") gets none of its icon.
    #[test]
    fn a_new_groups_icon_follows_its_name() {
        assert_eq!(guess_icon("Agents"), ChatGroupIcon::Sparkles);
        assert_eq!(guess_icon("AI bridge"), ChatGroupIcon::Sparkles);
        assert_eq!(guess_icon("Work"), ChatGroupIcon::Briefcase);
        assert_eq!(guess_icon("Family & friends"), ChatGroupIcon::People);
        assert_eq!(guess_icon("Seedwell development"), ChatGroupIcon::Hammer);
        assert_eq!(guess_icon("Aid"), ChatGroupIcon::Folder);
        assert_eq!(guess_icon("Misc"), ChatGroupIcon::Tray);
        assert_eq!(guess_icon("Plans"), ChatGroupIcon::Folder);
    }

    /// Catches an empty group name taken, and a rename to the name it has running anything.
    #[test]
    fn the_group_dialogs_ask_for_a_name() {
        use crate::chat_dialogs::{chat_dialog_check, ChatDialogInput};
        let new = chat_new_group_dialog();
        assert_eq!((new.cancel_title.as_str(), new.confirm_title.as_str()), ("Cancel", "Start"));
        assert_eq!(chat_dialog_check(new.clone(), "  ".into()), ChatDialogInput::Problem { text: "Give the group a name.".into() });
        assert_eq!(chat_dialog_check(new, " Agents ".into()), ChatDialogInput::Ready { value: Some("Agents".into()) });
        let g = SyncChatGroup { id: "g".into(), name: "Agents".into(), icon: ChatGroupIcon::Sparkles };
        let rename = chat_rename_group_dialog(g);
        assert_eq!(rename.field.as_ref().map(|f| f.text.as_str()), Some("Agents"));
        assert_eq!(chat_dialog_check(rename.clone(), "Agents".into()), ChatDialogInput::Unchanged);
        assert_eq!(chat_dialog_check(rename, "Bots".into()), ChatDialogInput::Ready { value: Some("Bots".into()) });
    }
}
