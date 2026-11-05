//! The Share dialog of a synced folder or a channel (SPEC 7G.1, 7G.5) as a model every host draws
//! (docs/CROSS-PLATFORM.md §1): the settings of the next link (role, uses, how long it works, a chat it joins too) and
//! what they add up to, Copy Link, the sharing period (an owner starts, extends or lifts it; an admin sees it), the
//! active links with Revoke, and what the dialog says when this computer may not share or the folder is gone. Opened
//! on a folder not synced yet (the file manager's Share menu), Copy Link syncs it from this computer first. A folder's
//! dialog also has "On all my devices" and its "Read only" (SPEC 7G.1), applied at once; with Read only on, a new
//! link admits at Read only too.
//!
//! The host keeps a [`SyncShareForm`], reads the links (`Core::sync_invites`, off the main thread), draws
//! [`Core::sync_share_window`] and runs [`Core::sync_share_copy_link`], `Core::sync_revoke_invite` and
//! `Core::sync_set_share_until` (with `sync_sharing_end` / `sync_sharing_extended`) and [`Core::sync_share_set_devices`].

use std::path::Path;
use std::sync::Arc;

use trs_platform::computer;

use crate::devices_text::FollowLine;
use crate::library::{local_day, LocalClock};
use crate::sync::{SyncChannel, SyncInvite, SyncRole};
use crate::sync_text::{
    role, sync_active_links, sync_can_share, sync_invite_detail, sync_invite_line, sync_link_limits, sync_role_note, sync_share_day_choices,
    sync_share_new_note, sync_share_roles, sync_share_summary, sync_share_use_choices, sync_sharing_countdown, sync_sharing_extend_choices,
    sync_sharing_line, SyncChoice,
};
use crate::types::CoreError;
use crate::Core;

/// "Link copied" stays this long after Copy Link.
const COPIED_MS: u32 = 3_000;
/// An open dialog with a sharing period asks again this often (its countdown).
const COUNTDOWN_MS: u32 = 60_000;

/// What is chosen in the Share dialog. The host keeps it as a value and edits the plain fields itself.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncShareForm {
    /// The folder or channel (empty while `new_path` is not synced yet).
    pub id: String,
    /// A folder Copy Link syncs from this computer first (`None` once it is synced, or for a synced one).
    pub new_path: Option<String>,
    /// The role the next link admits at.
    pub role: SyncRole,
    /// One use (the default), else `uses` (0 = unlimited).
    pub single_use: bool,
    pub uses: u32,
    /// The link stops admitting after `link_days` (on by default, 7 days).
    pub link_expires: bool,
    pub link_days: u32,
    /// The days of a sharing period an owner starts (applied when switched on).
    pub share_days: u32,
    /// A folder's link also joins a channel: `chat`, or a new channel named after the folder (`chat` empty). On when
    /// the folder has a chat already.
    pub with_chat: bool,
    pub chat: String,
}

/// A role of the switch: the role and how it reads.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncShareRoleChoice {
    pub role: SyncRole,
    pub title: String,
}

/// A setting of the dialog: its name on the left and its switch on the right, and beside the switch a menu of
/// `choices` whenever there are any. The core leaves them out while the menu must not show: Single use's uses while it
/// is on, Link expires' days while it is off. Stop sharing after shows its days with the switch off.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncShareSetting {
    pub label: String,
    pub help: String,
    pub on: bool,
    pub choices: Vec<SyncChoice>,
    /// The value chosen in `choices`.
    pub value: u32,
}

/// A channel the link can join too: its id (empty: a new channel named after the folder) and how it reads.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncShareChatChoice {
    pub id: String,
    pub title: String,
}

/// "Include a chat": the switch, and while it is on, the channels (the new one first, a separator, then the channels
/// this computer may share).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncShareChat {
    pub label: String,
    pub help: String,
    pub on: bool,
    pub choices: Vec<SyncShareChatChoice>,
    pub chosen: String,
}

/// The folder's sharing period.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncShareSharing {
    /// A period runs: "Sharing ends in 6 days, 23 hours".
    pub countdown: Option<String>,
    /// An owner's Extend menu while it runs: "+ 1 day"…, then "No End Date" (`sync_set_share_until(id, 0)`); the days
    /// extend it with `sync_sharing_extended`.
    pub extend_title: Option<String>,
    pub extend_help: String,
    pub extend_choices: Vec<SyncChoice>,
    pub no_end_title: String,
    /// An owner without a period: "Stop sharing after" with its days; switching it on starts the period at once
    /// (`sync_sharing_end(share_days)`).
    pub start: Option<SyncShareSetting>,
    /// What it means, under it.
    pub line: String,
}

/// An active link: its line, the grey line under it, and Revoke.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncShareLink {
    pub id: String,
    pub line: String,
    pub detail: String,
}

/// The Share dialog as drawn.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncShareWindowModel {
    pub caption: String,
    /// "Share “Photos”".
    pub title: String,
    /// Instead of the settings: it is gone, or this computer may not share it.
    pub gone: Option<String>,
    pub refusal: Option<String>,
    /// A folder not synced yet: what Copy Link does first.
    pub new_note: Option<String>,
    /// The role switch, its value and what the chosen role may do (its tooltip).
    pub roles: Vec<SyncShareRoleChoice>,
    pub role: SyncRole,
    pub role_help: String,
    pub single_use: SyncShareSetting,
    pub link_expires: SyncShareSetting,
    /// A folder's: the channel the link joins too.
    pub chat: Option<SyncShareChat>,
    /// "One Mac can join with this link within 7 days, as Read & write."
    pub summary: String,
    /// The sharing period of a folder or channel this computer may share (not before the folder is synced).
    pub sharing: Option<SyncShareSharing>,
    /// A folder's "On all my devices" and its "Read only" (while this computer is paired with others and may share
    /// it), as they are: switching either applies at once (`Core::sync_share_set_devices`, off the main thread).
    pub devices: Option<FollowLine>,
    /// "Active links", the links, "and 2 more", and each one's Revoke and its tooltip.
    pub links_caption: String,
    pub links: Vec<SyncShareLink>,
    pub links_more: Option<String>,
    pub revoke_title: String,
    pub revoke_help: String,
    /// "Cancel" (a folder not synced yet: it stays unsynced) or "Done", and its tooltip.
    pub close_title: String,
    pub close_help: String,
    /// "Copy Link" while this computer may share, its tooltip; "Link copied" for `copied_ms` after it.
    pub copy_title: Option<String>,
    pub copy_help: String,
    pub copied_title: String,
    pub copied_ms: u32,
    /// Ask for the model again after this long (the countdown); `None`: only after a change.
    pub refresh_ms: Option<u32>,
}

/// What Copy Link made: the link (a secret: copied, never shown or logged), and the form as it is now (the folder
/// synced, the chat it joins).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncShareCopied {
    pub invite: SyncInvite,
    pub form: SyncShareForm,
}

/// What is shared.
#[derive(Clone)]
struct Target {
    name: String,
    role: SyncRole,
    share_until_s: Option<u64>,
    is_channel: bool,
    channel_id: Option<String>,
}

fn form(id: String, new_path: Option<String>) -> SyncShareForm {
    SyncShareForm {
        id,
        new_path,
        role: SyncRole::ReadWrite,
        single_use: true,
        uses: 5,
        link_expires: true,
        link_days: 7,
        share_days: 7,
        with_chat: false,
        chat: String::new(),
    }
}

/// The channels a folder's link can join (this computer may share them).
fn chat_choices(channels: &[SyncChannel]) -> Vec<&SyncChannel> {
    channels.iter().filter(|c| sync_can_share(c.role)).collect()
}

/// The channel the next link joins too, by name.
fn chat_name(f: &SyncShareForm, t: &Target, channels: &[SyncChannel]) -> Option<String> {
    if !f.with_chat || t.is_channel {
        return None;
    }
    if f.chat.is_empty() { Some(t.name.clone()) } else { channels.iter().find(|c| c.id == f.chat).map(|c| c.name.clone()) }
}

fn setting(label: &str, help: &str, on: bool, choices: Vec<SyncChoice>, value: u32) -> SyncShareSetting {
    SyncShareSetting { label: label.into(), help: help.into(), on, choices, value }
}

#[allow(clippy::too_many_arguments)]
fn window(
    f: &SyncShareForm,
    t: Option<&Target>,
    channels: &[SyncChannel],
    invites: Vec<SyncInvite>,
    now_s: u64,
    clock: &dyn LocalClock,
    follow: Option<FollowLine>,
) -> SyncShareWindowModel {
    let can_share = t.is_some_and(|t| sync_can_share(t.role));
    let limits = sync_link_limits(f.single_use, f.uses, f.link_expires, f.link_days);
    let owner = t.is_some_and(|t| t.role == SyncRole::Owner);
    let sharing = t.filter(|_| can_share && f.new_path.is_none()).map(|t| {
        let until = t.share_until_s;
        SyncShareSharing {
            countdown: until.map(|u| sync_sharing_countdown(u, now_s)),
            extend_title: (owner && until.is_some()).then(|| "Extend".into()),
            extend_help: "Share for longer, from the current end; or share with no end".into(),
            extend_choices: sync_sharing_extend_choices(),
            no_end_title: "No End Date".into(),
            start: (owner && until.is_none()).then(|| {
                setting(
                    "Stop sharing after",
                    concat!("When the time is up, every ", computer!(), " but yours leaves the folder (each keeps the files it has), and links stop working"),
                    false,
                    sync_share_day_choices(),
                    f.share_days,
                )
            }),
            line: sync_sharing_line(until.map(|u| local_day(clock, u, now_s)), owner),
        }
    });
    let active = if can_share && f.new_path.is_none() { sync_active_links(invites, now_s) } else { sync_active_links(Vec::new(), now_s) };
    let new = f.new_path.is_some();
    SyncShareWindowModel {
        caption: "Folder Sync".into(),
        title: format!("Share “{}”", t.map_or("Folder", |t| t.name.as_str())),
        gone: t.is_none().then(|| concat!("This is no longer on this ", computer!(), ".").into()),
        refusal: (t.is_some() && !can_share).then(|| "Only the owner and admins can share it. Ask one of them for a link.".into()),
        new_note: f.new_path.as_deref().map(|p| sync_share_new_note(trs_platform::dirs::shown(Path::new(p)))),
        roles: sync_share_roles(t.map_or(SyncRole::ReadOnly, |t| t.role)).into_iter().map(|r| SyncShareRoleChoice { role: r, title: role(r).into() }).collect(),
        role: f.role,
        role_help: sync_role_note(f.role),
        single_use: setting(
            "Single use",
            concat!("The link stops working once one ", computer!(), " has joined with it"),
            f.single_use,
            if f.single_use { Vec::new() } else { sync_share_use_choices() },
            f.uses,
        ),
        link_expires: setting(
            "Link expires",
            "How long the link can be used to join",
            f.link_expires,
            if f.link_expires { sync_share_day_choices() } else { Vec::new() },
            f.link_days,
        ),
        chat: t.filter(|t| !t.is_channel).map(|t| SyncShareChat {
            label: "Include a chat".into(),
            help: "The same link also joins a channel, for the people and agents of this folder".into(),
            on: f.with_chat,
            choices: std::iter::once(SyncShareChatChoice { id: String::new(), title: format!("New channel “{}”", t.name) })
                .chain(chat_choices(channels).into_iter().map(|c| SyncShareChatChoice { id: c.id.clone(), title: c.name.clone() }))
                .collect(),
            chosen: f.chat.clone(),
        }),
        summary: t.map(|t| sync_share_summary(f.role, limits.uses, limits.link_days, chat_name(f, t, channels))).unwrap_or_default(),
        sharing,
        devices: follow
            .filter(|l| l.shown && can_share && f.new_path.is_none() && t.is_some_and(|t| !t.is_channel))
            .map(|l| crate::sync_dialogs::follow_drawn(&l, l.on, l.read_only.as_ref().is_some_and(|r| r.on))),
        links_caption: "Active links".into(),
        links: active
            .links
            .iter()
            .map(|i| SyncShareLink {
                id: i.id.clone(),
                line: sync_invite_line(i.clone(), now_s),
                detail: sync_invite_detail(i.clone(), i.expires_s.map(|e| local_day(clock, e, now_s))),
            })
            .collect(),
        links_more: active.more,
        revoke_title: "Revoke".into(),
        revoke_help: "Stop this link from admitting anyone else".into(),
        close_title: if new { "Cancel" } else { "Done" }.into(),
        close_help: if new { "Close; the folder is not synced" } else { "Close; links keep working until used up, expired or revoked" }.into(),
        copy_title: can_share.then(|| "Copy Link".into()),
        copy_help: "Make a link with these settings and copy it".into(),
        copied_title: "Link copied".into(),
        copied_ms: COPIED_MS,
        refresh_ms: t.and_then(|t| t.share_until_s).map(|_| COUNTDOWN_MS),
    }
}

impl Core {
    fn share_target(&self, f: &SyncShareForm) -> Option<Target> {
        if let Some(p) = &f.new_path {
            let name = Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Some(Target { name, role: SyncRole::Owner, share_until_s: None, is_channel: false, channel_id: None });
        }
        if let Some(x) = self.sync_snapshot().folders.into_iter().find(|x| x.id == f.id) {
            return Some(Target { name: x.name, role: x.role, share_until_s: x.share_until_s, is_channel: false, channel_id: x.channel_id });
        }
        self.chat_channels()
            .into_iter()
            .find(|c| c.id == f.id)
            .map(|c| Target { name: c.name, role: c.role, share_until_s: c.share_until_s, is_channel: true, channel_id: None })
    }
}

impl Core {
    /// Copy Link: syncs a folder not synced yet first, makes the chat when a new one was chosen (named after the
    /// folder) and links it, then makes the link with the form's settings. The host copies `invite.link` (a secret:
    /// never shown or logged), shows "Link copied", keeps the returned form and reads the links again. Slow: off the
    /// main thread. A failure after the folder was synced leaves it synced (the error says what failed).
    pub fn sync_share_copy_link(&self, form: SyncShareForm) -> Result<SyncShareCopied, CoreError> {
        let t = self.share_target(&form).ok_or_else(|| CoreError::Failed { message: concat!("This is no longer on this ", computer!(), ".").into() })?;
        if !sync_can_share(t.role) {
            return Err(CoreError::Failed { message: "Only the owner and admins can share it. Ask one of them for a link.".into() });
        }
        let channels = self.chat_channels();
        let name = chat_name(&form, &t, &channels);
        let mut form = form;
        if let Some(path) = form.new_path.take() {
            form.id = self.sync_add_folder(path)?.id;
        }
        let limits = sync_link_limits(form.single_use, form.uses, form.link_expires, form.link_days);
        let mut channel = name.as_ref().map(|_| form.chat.clone()).filter(|c| !c.is_empty());
        if let (Some(n), true) = (&name, form.chat.is_empty()) {
            let c = self.chat_new(n.clone(), false)?.id;
            form.chat = c.clone();
            channel = Some(c);
        }
        if let Some(c) = &channel
            && t.channel_id.as_ref() != Some(c)
        {
            self.sync_set_channel(form.id.clone(), Some(c.clone()))?;
        }
        let invite = self.sync_create_invite(form.id.clone(), form.role, limits.uses, limits.link_days, channel)?;
        Ok(SyncShareCopied { invite, form })
    }

    /// The Share dialog's "On all my devices" (`on`) and "Read only" (`read_only`) switched: applied at once (Read only
    /// on also puts the folder on all devices; "On all my devices" off keeps Read only as it is). Returns the form with
    /// the next link's role: Read only once Read only comes on, Read & write again when it goes off. Slow: off the main
    /// thread. On failure the host shows `core_error_message` and draws the dialog again (the switches as they are).
    pub fn sync_share_set_devices(&self, form: SyncShareForm, on: bool, read_only: bool) -> Result<SyncShareForm, CoreError> {
        let line = self.sync_follow_line(form.id.clone());
        let was = line.read_only.as_ref().is_some_and(|r| r.on);
        if read_only != was && (on || read_only) {
            self.sync_set_follow_read_only(form.id.clone(), read_only)?;
        } else if on != line.on {
            self.sync_set_follow(form.id.clone(), on)?;
        }
        let now = self.sync_follow_line(form.id.clone()).read_only.is_some_and(|r| r.on);
        let role = match (was, now) {
            (false, true) => SyncRole::ReadOnly,
            (true, false) if form.role == SyncRole::ReadOnly => SyncRole::ReadWrite,
            _ => form.role,
        };
        Ok(SyncShareForm { role, ..form })
    }
}

#[uniffi::export]
impl Core {
    /// The Share dialog's form for folder or channel `id` as it opens: Read & write (Read only while the folder is
    /// Read only on the user's devices), single use, 7 days; a folder's link joins its chat too when it has one this
    /// computer may share.
    pub fn sync_share_form(&self, id: String) -> SyncShareForm {
        let mut f = form(id.clone(), None);
        if self.sync_follow_line(id).read_only.is_some_and(|r| r.on) {
            f.role = SyncRole::ReadOnly;
        }
        if let Some(t) = self.share_target(&f)
            && let Some(c) = t.channel_id
            && chat_choices(&self.chat_channels()).iter().any(|x| x.id == c)
        {
            f.with_chat = true;
            f.chat = c;
        }
        f
    }

    /// The Share dialog of a folder not synced yet (the file manager's Share menu): Copy Link syncs it from this
    /// computer first, as its owner.
    pub fn sync_share_form_new(&self, path: String) -> SyncShareForm {
        form(String::new(), Some(path))
    }

    /// The Share dialog drawn from `form`, with the links the host read (`Core::sync_invites`). Cheap: it reads the
    /// folders and channels, nothing slow.
    pub fn sync_share_window(&self, form: SyncShareForm, invites: Vec<SyncInvite>, now_unix_s: u64, clock: Arc<dyn LocalClock>) -> SyncShareWindowModel {
        let follow = (form.new_path.is_none() && !form.id.is_empty()).then(|| self.sync_follow_line(form.id.clone()));
        window(&form, self.share_target(&form).as_ref(), &self.chat_channels(), invites, now_unix_s, clock.as_ref(), follow)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::{mock, Clock};

    const NOW_S: u64 = 1_800_000_000;

    #[test]
    fn a_folder_s_dialog_has_its_settings_summary_and_sharing_period() {
        let (core, _d) = mock();
        let f = core.sync_add_folder("/tmp/seedwell-mock/Notes".into()).unwrap();
        let form = core.sync_share_form(f.id.clone());
        let w = core.sync_share_window(form.clone(), Vec::new(), NOW_S, Arc::new(Clock));
        let computer = computer!();
        assert_eq!((w.caption.as_str(), w.title.as_str(), w.gone.clone(), w.refusal.clone()), ("Folder Sync", "Share “Notes”", None, None));
        assert_eq!(w.roles.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), ["Read & write", "Read only", "Admin"]);
        assert_eq!((w.role, w.role_help.clone()), (SyncRole::ReadWrite, sync_role_note(SyncRole::ReadWrite)));
        assert_eq!((w.single_use.label.as_str(), w.single_use.on), ("Single use", true));
        assert!(w.single_use.choices.is_empty(), "one use: no menu of uses");
        assert_eq!((w.link_expires.label.as_str(), w.link_expires.on, w.link_expires.value), ("Link expires", true, 7));
        assert_eq!(w.link_expires.choices, sync_share_day_choices());
        assert_eq!(w.summary, format!("One {computer} can join with this link within 7 days, as Read & write."));
        let chat = w.chat.clone().unwrap();
        assert!(!chat.on && chat.choices[0] == SyncShareChatChoice { id: String::new(), title: "New channel “Notes”".into() });
        let sharing = w.sharing.clone().unwrap();
        assert_eq!((sharing.countdown.clone(), sharing.extend_title.clone()), (None, None));
        let start = sharing.start.unwrap();
        assert_eq!((start.label.as_str(), start.on, start.value), ("Stop sharing after", false, 7));
        assert_eq!(start.choices, sync_share_day_choices(), "its days show with the switch off");
        assert_eq!(sharing.line, sync_sharing_line(None, true));
        assert_eq!((w.close_title.as_str(), w.copy_title.as_deref(), w.refresh_ms), ("Done", Some("Copy Link"), None));
        // With a chat: the summary says so.
        let with_chat = SyncShareForm { with_chat: true, single_use: false, uses: 0, link_expires: false, ..form.clone() };
        let w = core.sync_share_window(with_chat.clone(), Vec::new(), NOW_S, Arc::new(Clock));
        assert_eq!(w.single_use.choices, sync_share_use_choices(), "more than one use: how many");
        assert!(w.link_expires.choices.is_empty(), "no end: no menu of days");
        assert_eq!(w.summary, format!("Any number of {computer}s can join with this link, as Read & write. It joins the channel “Notes” too."));
        // Copy Link: a new channel named after the folder, linked to it; the form keeps that channel.
        let copied = core.sync_share_copy_link(with_chat).unwrap();
        assert!(copied.invite.link.starts_with("seedwell://sync/"));
        assert!(!copied.form.chat.is_empty());
        let now = core.sync_snapshot().folders.into_iter().find(|x| x.id == f.id).unwrap();
        assert_eq!(now.channel_id.as_deref(), Some(copied.form.chat.as_str()));
        assert_eq!(core.sync_share_form(f.id.clone()).chat, copied.form.chat, "the next opening starts from the folder's chat");
        // The links: newest first, with Revoke.
        let invites = core.sync_invites(f.id.clone());
        let w = core.sync_share_window(copied.form, invites, NOW_S, Arc::new(Clock));
        assert_eq!(w.links.len(), 1);
        assert_eq!((w.links_caption.as_str(), w.revoke_title.as_str()), ("Active links", "Revoke"));
        // A sharing period: the countdown, Extend for the owner, and a refresh every minute.
        core.sync_set_share_until(f.id.clone(), NOW_S + 86_400 + 60).unwrap();
        let w = core.sync_share_window(form, Vec::new(), NOW_S, Arc::new(Clock));
        let s = w.sharing.unwrap();
        assert_eq!(s.countdown.as_deref(), Some("Sharing ends in 1 day, 0 hours"));
        assert_eq!((s.extend_title.as_deref(), s.start), (Some("Extend"), None));
        assert_eq!(s.line, sync_sharing_line(Some(format!("D{}", NOW_S + 86_400 + 60)), true));
        assert_eq!(w.refresh_ms, Some(COUNTDOWN_MS));
    }

    /// The Share dialog's "On all my devices" and "Read only": shown for a folder this computer may share, while it is
    /// paired, as the core has them (they apply at once, so the dialog must never show a choice that is not applied);
    /// never for a channel, a folder this computer may not share, or one not synced yet.
    #[test]
    fn the_share_dialog_shows_the_devices_switches_as_they_are() {
        let line = FollowLine { on: true, ..crate::devices_text::follow_line_new(&["MacBook".into()], false) };
        let line = FollowLine { read_only: line.read_only.map(|r| crate::devices_text::FollowReadOnly { on: true, ..r }), ..line };
        let folder = Target { name: "Films".into(), role: SyncRole::Owner, share_until_s: None, is_channel: false, channel_id: None };
        let w = |f: &SyncShareForm, t: &Target| window(f, Some(t), &[], Vec::new(), NOW_S, &Clock, Some(line.clone())).devices;
        let f = form("x".into(), None);
        let d = w(&f, &folder).expect("a folder's dialog has them");
        let r = d.read_only.unwrap();
        assert!(d.on && r.on && r.enabled && r.line.contains("never change, add or delete"), "{r:?}");
        assert_eq!(w(&f, &Target { is_channel: true, ..folder.clone() }), None, "not for a channel");
        assert_eq!(w(&f, &Target { role: SyncRole::ReadOnly, ..folder.clone() }), None, "not where this computer may not share");
        assert_eq!(w(&form(String::new(), Some("/tmp/x".into())), &folder), None, "not before the folder is synced");
    }

    #[test]
    fn a_folder_not_synced_yet_is_synced_by_copy_link() {
        let (core, _d) = mock();
        let form = core.sync_share_form_new("/tmp/seedwell-mock/Novel".into());
        let w = core.sync_share_window(form.clone(), Vec::new(), NOW_S, Arc::new(Clock));
        assert_eq!(w.title, "Share “Novel”");
        assert_eq!(w.new_note, Some(sync_share_new_note("/tmp/seedwell-mock/Novel".into())));
        assert_eq!((w.close_title.as_str(), w.sharing.is_none()), ("Cancel", true), "no sharing period before it is synced");
        let copied = core.sync_share_copy_link(form).unwrap();
        assert!(copied.form.new_path.is_none() && !copied.form.id.is_empty());
        assert!(core.sync_snapshot().folders.iter().any(|f| f.id == copied.form.id && f.name == "Novel"));
    }

    #[test]
    fn a_member_who_may_not_share_is_told_so() {
        let (core, _d) = mock();
        let ro = core.sync_snapshot().folders.into_iter().find(|f| !sync_can_share(f.role)).expect("a mock folder this computer may not share");
        let form = core.sync_share_form(ro.id.clone());
        let w = core.sync_share_window(form.clone(), Vec::new(), NOW_S, Arc::new(Clock));
        assert_eq!(w.refusal.as_deref(), Some("Only the owner and admins can share it. Ask one of them for a link."));
        assert_eq!((w.copy_title, w.sharing), (None, None));
        assert!(core.sync_share_copy_link(form).is_err());
        let gone = core.sync_share_window(core.sync_share_form("nothing".into()), Vec::new(), NOW_S, Arc::new(Clock));
        assert_eq!((gone.title.as_str(), gone.gone), ("Share “Folder”", Some(format!("This is no longer on this {}.", computer!()))));
    }
}
