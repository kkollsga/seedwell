//! Groups of channels in this device's sidebar (SPEC §7G.7 *Groups*): a name and an icon, in the user's order, that
//! channels are listed under. They are this device's own, like a channel's Mute: kept in `sync.toml`
//! (`[[chat_group]]`, and each channel's `group`), never signed into a channel or sent to another device. Deleting a
//! group returns its channels to the plain list and loses nothing; leaving a channel takes its entry, and with it its
//! place in a group; a group with no channels stays until the user deletes it.

use seedwell_sync_proto::ids::FolderId;

use crate::chat::clean_name;
use crate::config::ChatGroupEntry;
use crate::service::{SyncError, SyncHandle, invalid, random_id};
use crate::shared::lock;

/// The icons a group can have, by name, in the order a picker lists them. The hosts draw each with their own symbol.
pub const GROUP_ICONS: [&str; 12] = ["sparkles", "folder", "briefcase", "house", "people", "star", "hammer", "book", "gamecontroller", "heart", "globe", "tray"];

/// The icon of a group made without one.
pub const DEFAULT_GROUP_ICON: &str = "folder";

/// The longest group name kept, in characters.
pub const GROUP_NAME_MAX: usize = 64;

/// A group of channels, for the UI and the agent interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatGroupStatus {
    /// Its id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Its icon's name ([`GROUP_ICONS`]).
    pub icon: String,
}

fn status_of(e: &ChatGroupEntry) -> ChatGroupStatus {
    let icon = if GROUP_ICONS.contains(&e.icon.as_str()) { e.icon.clone() } else { DEFAULT_GROUP_ICON.to_owned() };
    ChatGroupStatus { id: e.id.clone(), name: e.name.clone(), icon }
}

/// The icon a new group gets from its name when none was picked: "Agents" sparkles, "Work" a briefcase, "Family"
/// people; else [`DEFAULT_GROUP_ICON`]. The app's New Group… and `chat.group_new` without an icon both take it.
pub fn icon_for_name(name: &str) -> &'static str {
    const WORDS: &[(&[&str], &str)] = &[
        (&["agent", "bot", "ai", "claude", "assistant"], "sparkles"),
        (&["work", "job", "office", "client", "business"], "briefcase"),
        (&["home", "house", "cabin", "hytta"], "house"),
        (&["family", "friend", "people", "team", "kids"], "people"),
        (&["dev", "build", "code", "project", "engineering"], "hammer"),
        (&["book", "read", "study", "school", "class"], "book"),
        (&["game", "gaming", "play"], "gamecontroller"),
        (&["love", "heart", "partner"], "heart"),
        (&["travel", "trip", "world", "global"], "globe"),
        (&["inbox", "misc", "other", "archive"], "tray"),
        (&["star", "favorite", "favourite", "important"], "star"),
    ];
    let lower = name.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    // A short key ("ai") is only a whole word: "aid" is not one.
    WORDS
        .iter()
        .find(|(keys, _)| words.iter().any(|w| keys.iter().any(|k| w.starts_with(k) && (k.len() > 2 || w.len() == k.len()))))
        .map_or(DEFAULT_GROUP_ICON, |(_, icon)| *icon)
}

/// `icon` as a known icon's name, or why it is none; empty: the one the group's `name` suggests.
pub fn group_icon_for(icon: &str, name: &str) -> Result<&'static str, SyncError> {
    if icon.trim().is_empty() {
        return Ok(icon_for_name(name));
    }
    group_icon(icon)
}

/// `icon` as a known icon's name (empty: the default), or why it is none.
pub fn group_icon(icon: &str) -> Result<&'static str, SyncError> {
    let icon = icon.trim().to_lowercase();
    if icon.is_empty() {
        return Ok(DEFAULT_GROUP_ICON);
    }
    GROUP_ICONS.iter().copied().find(|i| *i == icon).ok_or_else(|| invalid(format!("`{icon}` is not a group icon; one of: {}", GROUP_ICONS.join(", "))))
}

/// A group that is not (or no longer) here.
fn no_group() -> SyncError {
    invalid("no such group")
}

fn group_name(name: &str) -> Result<String, SyncError> {
    clean_name(name).map(|n| n.chars().take(GROUP_NAME_MAX).collect()).ok_or_else(|| invalid("a group needs a name"))
}

impl SyncHandle {
    /// The sidebar's groups of channels, in the user's order. Cheap.
    pub fn chat_groups(&self) -> Vec<ChatGroupStatus> {
        lock(&self.inner.cfg).chat_groups.iter().map(status_of).collect()
    }

    /// A new group, last in the order. Its name is trimmed and must be new (in any case); `icon` empty: the one its
    /// name suggests ([`icon_for_name`]).
    pub fn chat_group_new(&self, name: &str, icon: &str) -> Result<ChatGroupStatus, SyncError> {
        let name = group_name(name)?;
        let icon = group_icon_for(icon, &name)?;
        let entry = {
            let mut cfg = lock(&self.inner.cfg);
            if cfg.chat_groups.iter().any(|g| g.name.to_lowercase() == name.to_lowercase()) {
                return Err(invalid(format!("there is already a group named “{name}”")));
            }
            let id = seedwell_sync_proto::members::hex(&random_id()[..6]);
            let e = ChatGroupEntry { id, name, icon: icon.to_owned() };
            cfg.chat_groups.push(e.clone());
            e
        };
        self.save_config()?;
        Ok(status_of(&entry))
    }

    /// Renames a group and/or changes its icon (`None`: as it is).
    pub fn chat_group_edit(&self, id: &str, name: Option<&str>, icon: Option<&str>) -> Result<ChatGroupStatus, SyncError> {
        let name = name.map(group_name).transpose()?;
        let icon = icon.map(group_icon).transpose()?;
        let entry = {
            let mut cfg = lock(&self.inner.cfg);
            if let Some(n) = &name
                && cfg.chat_groups.iter().any(|g| g.id != id && g.name.to_lowercase() == n.to_lowercase())
            {
                return Err(invalid(format!("there is already a group named “{n}”")));
            }
            let g = cfg.chat_groups.iter_mut().find(|g| g.id == id).ok_or_else(no_group)?;
            if let Some(n) = name {
                g.name = n;
            }
            if let Some(i) = icon {
                g.icon = i.to_owned();
            }
            g.clone()
        };
        self.save_config()?;
        Ok(status_of(&entry))
    }

    /// Moves a group to `index` in the order (0: first; past the end: last).
    pub fn chat_group_move(&self, id: &str, index: usize) -> Result<(), SyncError> {
        {
            let mut cfg = lock(&self.inner.cfg);
            let from = cfg.chat_groups.iter().position(|g| g.id == id).ok_or_else(no_group)?;
            let g = cfg.chat_groups.remove(from);
            let to = index.min(cfg.chat_groups.len());
            cfg.chat_groups.insert(to, g);
        }
        self.save_config()?;
        Ok(())
    }

    /// Deletes a group: its channels go back to the plain list, as they are (their topics, settings and messages
    /// untouched).
    pub fn chat_group_delete(&self, id: &str) -> Result<(), SyncError> {
        {
            let mut cfg = lock(&self.inner.cfg);
            let before = cfg.chat_groups.len();
            cfg.chat_groups.retain(|g| g.id != id);
            if cfg.chat_groups.len() == before {
                return Err(no_group());
            }
            for c in cfg.channels.iter_mut().filter(|c| c.group == id) {
                c.group.clear();
            }
        }
        for c in self.inner.ctx.registry.channels() {
            let mut s = lock(&c.settings);
            if s.group == id {
                s.group.clear();
            }
        }
        self.save_config()?;
        Ok(())
    }

    /// Lists a channel under group `group` in the sidebar (`None`: under none, in the plain list).
    pub fn chat_set_group(&self, channel: &FolderId, group: Option<&str>) -> Result<(), SyncError> {
        let c = self.channel(channel)?;
        let group = group.unwrap_or_default();
        {
            let mut cfg = lock(&self.inner.cfg);
            if !group.is_empty() && !cfg.chat_groups.iter().any(|g| g.id == group) {
                return Err(no_group());
            }
            if let Some(e) = cfg.channel_mut(&channel.to_string()) {
                e.group = group.to_owned();
            }
        }
        lock(&c.settings).group = group.to_owned();
        self.save_config()?;
        Ok(())
    }
}
