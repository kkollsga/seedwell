//! The Share dialog of a synced folder or a channel (SPEC 7G.1, 7G.5) as a model every host draws
//! (docs/CROSS-PLATFORM.md §1): the settings of the next link (role, uses, how long it works, a chat it joins too) and
//! what they add up to, Copy Link, the sharing period (an owner starts, extends or lifts it; an admin sees it), the
//! active links with Revoke, and what the dialog says when this computer may not share or the folder is gone. Opened
//! on a folder not synced yet (the file manager's Share menu), Copy Link syncs it from this computer first. A folder's
//! dialog also has "On all my devices" and, on its own, "Read only" (SPEC 7G.1), each applied at once; with Read only
//! on, a new link admits at Read only too.
//!
//! The host keeps a [`SyncShareForm`], reads the links (`Core::sync_invites`, off the main thread), draws
//! [`Core::sync_share_window`] and runs [`Core::sync_share_copy_link`], `Core::sync_revoke_invite` and
//! `Core::sync_set_share_until` (with `sync_sharing_end` / `sync_sharing_extended`), [`Core::sync_share_set_devices`] and
//! [`Core::sync_share_set_read_only`].

use std::path::Path;
use std::sync::Arc;

use seedwell_platform::computer;

use crate::devices_text::FollowLine;
use crate::read_only_text::ReadOnlyLine;
use crate::library::{local_day, LocalClock};
use crate::sync::{SyncInvite, SyncRole};
use crate::sync_text::{
    role, sync_active_links, sync_can_share, sync_invite_detail, sync_invite_line, sync_link_limits, sync_role_note, sync_share_day_choices,
    sync_share_new_note, sync_share_roles, sync_share_summary, sync_share_use_choices, sync_sharing_countdown, sync_sharing_extend_choices,
    sync_sharing_extended, sync_sharing_line, SyncChoice,
};
use crate::types::CoreError;
use crate::Core;

/// The Share dialog's form of folder or channel `id` while it opens: the settings a link starts with, nothing read
/// (pure). The host shows the dialog as opening with it and reads [`Core::sync_share_form`] off its UI thread
/// meanwhile.
#[uniffi::export]
pub fn sync_share_form_opening(id: String) -> SyncShareForm {
    form(id, None)
}

/// "Link copied" stays this long after Copy Link.
const COPIED_MS: u32 = 3_000;
/// An open dialog of a synced folder or channel asks again this often: its sharing period's countdown, and what changed
/// on another device or in another window meanwhile (the folder removed, this computer's role, Read only or On all my
/// devices switched), which nothing in the dialog itself would redraw.
const REFRESH_MS: u32 = 60_000;

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
    /// A folder's "On all my devices" (while this computer is paired with others and may share it), as it is:
    /// switching it applies at once (`Core::sync_share_set_devices`, off the main thread).
    pub devices: Option<FollowLine>,
    /// A folder's "Read only" (once it is synced), as it is, on its own: switchable on an owner only, where switching
    /// applies at once (`Core::sync_share_set_read_only`, off the main thread); the state and who can switch it for
    /// anyone else.
    pub read_only: Option<ReadOnlyLine>,
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
    /// Read the links and ask for the model again after this long, while the dialog is open (the countdown, changes made
    /// elsewhere); `None` (a folder not synced yet, or gone): only after a change in the dialog. Off the UI thread, and
    /// drawn only when it differs.
    pub refresh_ms: Option<u32>,
}

/// What Copy Link made: the link (a secret: copied, never shown or logged), and the form as it is now (the folder
/// synced, the chat it joins).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncShareCopied {
    pub invite: SyncInvite,
    pub form: SyncShareForm,
}

/// What is shared, as the dialog shows it ([`crate::sync::SyncBackend::share_target`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareTarget {
    pub(crate) name: String,
    pub(crate) role: SyncRole,
    pub(crate) share_until_s: Option<u64>,
    pub(crate) is_channel: bool,
    pub(crate) channel_id: Option<String>,
}

/// A channel a folder's link may join too, as the dialog names it ([`crate::sync::SyncBackend::share_channels`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareChannel {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) role: SyncRole,
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
fn chat_choices(channels: &[ShareChannel]) -> Vec<&ShareChannel> {
    channels.iter().filter(|c| sync_can_share(c.role)).collect()
}

/// The channel the next link joins too, by name.
fn chat_name(f: &SyncShareForm, t: &ShareTarget, channels: &[ShareChannel]) -> Option<String> {
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
    t: Option<&ShareTarget>,
    channels: &[ShareChannel],
    invites: Vec<SyncInvite>,
    now_s: u64,
    clock: &dyn LocalClock,
    follow: Option<FollowLine>,
    read_only: Option<ReadOnlyLine>,
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
        new_note: f.new_path.as_deref().map(|p| sync_share_new_note(seedwell_platform::dirs::shown(Path::new(p)))),
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
        devices: follow.filter(|l| l.shown && can_share && f.new_path.is_none() && t.is_some_and(|t| !t.is_channel)),
        read_only: read_only.filter(|_| can_share && f.new_path.is_none() && t.is_some_and(|t| !t.is_channel)),
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
        refresh_ms: t.filter(|_| !new).map(|_| REFRESH_MS),
    }
}

impl Core {
    fn share_target(&self, f: &SyncShareForm) -> Option<ShareTarget> {
        if let Some(p) = &f.new_path {
            let name = Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Some(ShareTarget { name, role: SyncRole::Owner, share_until_s: None, is_channel: false, channel_id: None });
        }
        // That one folder or channel alone: never the status of every folder (the dialog is drawn on the UI thread).
        self.backend.sync().share_target(&f.id)
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
        let channels = self.backend.sync().share_channels();
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

    /// The Share dialog's Extend: the sharing period of folder or channel `id` ends `days` later than it does now (or
    /// `days` from now once it ended; without a period, one of `days` starts now). Owners only; applies at once, for
    /// every device. Slow (it saves): off the main thread.
    pub fn sync_share_extend(&self, id: String, days: u32, now_unix_s: u64) -> Result<(), CoreError> {
        let until = self.backend.sync().share_target(&id).and_then(|t| t.share_until_s);
        self.sync_set_share_until(id, sync_sharing_extended(until, days, now_unix_s))
    }

    /// The Share dialog's "On all my devices" switched (`on`): applied at once. Slow: off the main thread. On failure
    /// the host shows `core_error_message` and draws the dialog again (the switch as it is).
    pub fn sync_share_set_devices(&self, form: SyncShareForm, on: bool) -> Result<SyncShareForm, CoreError> {
        if on != self.sync_follow_line(form.id.clone()).on {
            self.sync_set_follow(form.id.clone(), on)?;
        }
        Ok(form)
    }

    /// The Share dialog's "Read only" (the owner's setting) switched (`on`): applied at once. Returns the form with the
    /// next link's role: Read only once Read only comes on, Read & write again when it goes off. Slow: off the main
    /// thread. On failure the host shows `core_error_message` and draws the dialog again (the switch as it is).
    pub fn sync_share_set_read_only(&self, form: SyncShareForm, on: bool) -> Result<SyncShareForm, CoreError> {
        let is_on = |core: &Core| crate::read_only_text::read_only_line_for(core.backend.as_ref(), &form.id).is_some_and(|r| r.on);
        let was = is_on(self);
        if on != was {
            self.sync_set_read_only(form.id.clone(), on)?;
        }
        let now = is_on(self);
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
    /// Read only), single use, 7 days; a folder's link joins its chat too when it has one this
    /// computer may share.
    pub fn sync_share_form(&self, id: String) -> SyncShareForm {
        let mut f = form(id.clone(), None);
        if crate::read_only_text::read_only_line_for(self.backend.as_ref(), &id).is_some_and(|r| r.on) {
            f.role = SyncRole::ReadOnly;
        }
        if let Some(t) = self.share_target(&f)
            && let Some(c) = t.channel_id
            && chat_choices(&self.backend.sync().share_channels()).iter().any(|x| x.id == c)
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

    /// The Share dialog drawn from `form`, with the links the host read (`Core::sync_invites`). Cheap: it reads that
    /// folder or channel alone, its two switches and the channels' names (each lock held only to copy a field), never
    /// the status of every folder or a channel's messages; [`crate::ui_wait_tests`] times it while a big folder is
    /// added. A host still draws it after a change only (its form, its links, a switch, the countdown's
    /// `refresh_ms`), never once per frame of its UI.
    pub fn sync_share_window(&self, form: SyncShareForm, invites: Vec<SyncInvite>, now_unix_s: u64, clock: Arc<dyn LocalClock>) -> SyncShareWindowModel {
        let synced = form.new_path.is_none() && !form.id.is_empty();
        let follow = synced.then(|| self.sync_follow_line(form.id.clone()));
        let read_only = synced.then(|| crate::read_only_text::read_only_line_for(self.backend.as_ref(), &form.id)).flatten();
        window(&form, self.share_target(&form).as_ref(), &self.backend.sync().share_channels(), invites, now_unix_s, clock.as_ref(), follow, read_only)
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
        assert_eq!((w.close_title.as_str(), w.copy_title.as_deref(), w.refresh_ms), ("Done", Some("Copy Link"), Some(REFRESH_MS)));
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
        assert_eq!(w.refresh_ms, Some(REFRESH_MS));
    }

    /// The Share dialog's "On all my devices" (while paired) and "Read only" (on its own: also on a computer paired with
    /// nothing), as the core has them (they apply at once, so the dialog must never show a choice that is not applied);
    /// never for a channel, a folder this computer may not share, or one not synced yet. Fails when Read only is missing
    /// without pairing, or shown where it must not be.
    #[test]
    fn the_share_dialog_shows_the_devices_switches_as_they_are() {
        let line = FollowLine { on: true, ..crate::devices_text::follow_line_new(&["MacBook".into()], false) };
        let ro = crate::read_only_text::read_only_line(true, true);
        let folder = ShareTarget { name: "Films".into(), role: SyncRole::Owner, share_until_s: None, is_channel: false, channel_id: None };
        let w = |f: &SyncShareForm, t: &ShareTarget, l: &FollowLine| window(f, Some(t), &[], Vec::new(), NOW_S, &Clock, Some(l.clone()), Some(ro.clone()));
        let f = form("x".into(), None);
        let d = w(&f, &folder, &line);
        let (devices, r) = (d.devices.expect("a folder's dialog has it"), d.read_only.expect("and Read only"));
        assert!(devices.on && r.on && r.enabled && r.line.contains("change nothing in it"), "{r:?}");
        // Paired with nothing: no "On all my devices", Read only still.
        let alone = w(&f, &folder, &crate::devices_text::follow_line_new(&[], false));
        assert!(alone.devices.is_none() && alone.read_only.is_some(), "Read only went with On all my devices");
        for (t, why) in [(ShareTarget { is_channel: true, ..folder.clone() }, "a channel"), (ShareTarget { role: SyncRole::ReadOnly, ..folder.clone() }, "may not share")] {
            let x = w(&f, &t, &line);
            assert!(x.devices.is_none() && x.read_only.is_none(), "shown for {why}");
        }
        let new = w(&form(String::new(), Some("/tmp/x".into())), &folder, &line);
        assert!(new.devices.is_none() && new.read_only.is_none(), "shown before the folder is synced");
    }

    /// The Share dialog's Read only on an owner's folder, this computer paired with nothing: it is there, applies at once
    /// (the next link's role follows it, and so does the dialog's role when it opens again), and an admin sees it but is
    /// refused the switch. Fails when the switch is missing without pairing, does not apply, or a non-owner switches it.
    #[test]
    fn the_share_dialogs_read_only_applies_at_once_and_only_on_an_owner() {
        let (core, _d) = mock();
        let folders = core.sync_snapshot().folders;
        let owned = folders.iter().find(|f| f.role == SyncRole::Owner).unwrap().id.clone();
        let form = core.sync_share_form(owned.clone());
        let w = core.sync_share_window(form.clone(), Vec::new(), NOW_S, Arc::new(Clock));
        assert!(w.devices.is_none(), "the mock is paired with nothing");
        let r = w.read_only.expect("an unpaired owner's dialog has Read only");
        assert!(r.enabled && !r.on, "{r:?}");
        let form = core.sync_share_set_read_only(form, true).unwrap();
        assert_eq!(form.role, SyncRole::ReadOnly, "the next link follows Read only");
        assert!(core.sync_share_window(form.clone(), Vec::new(), NOW_S, Arc::new(Clock)).read_only.is_some_and(|r| r.on));
        assert_eq!(core.sync_share_form(owned.clone()).role, SyncRole::ReadOnly);
        let form = core.sync_share_set_read_only(form, false).unwrap();
        assert_eq!(form.role, SyncRole::ReadWrite);
        let admin = folders.iter().find(|f| f.role == SyncRole::Admin).unwrap().id.clone();
        let w = core.sync_share_window(core.sync_share_form(admin.clone()), Vec::new(), NOW_S, Arc::new(Clock));
        assert!(w.read_only.is_some_and(|r| !r.enabled), "an admin sees it, not switchable");
        assert!(core.sync_share_set_read_only(core.sync_share_form(admin), true).is_err(), "an admin switched it");
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

    /// The failure this targets: **an open Share dialog keeps a stale drawing** when its folder changes elsewhere (removed
    /// on another device, this computer demoted, Read only switched in another window): nothing in the dialog changes, so
    /// a host that draws after a change only never asks again. Every synced folder's dialog asks for a refresh, with or
    /// without a sharing period, and the next drawing shows the change; a folder not synced yet, or gone, has nothing to
    /// wait for.
    #[test]
    fn an_open_share_dialog_is_asked_again_so_changes_made_elsewhere_reach_it() {
        let (core, _d) = mock();
        let f = core.sync_add_folder("/tmp/seedwell-mock/Letters".into()).unwrap();
        let form = core.sync_share_form(f.id.clone());
        let draw = |form: &SyncShareForm| core.sync_share_window(form.clone(), core.sync_invites(form.id.clone()), NOW_S, Arc::new(Clock));
        let open = draw(&form);
        assert!(open.sharing.as_ref().is_some_and(|s| s.countdown.is_none()), "no sharing period: {open:?}");
        assert_eq!(open.refresh_ms, Some(REFRESH_MS), "a dialog with no countdown is never asked again");
        assert!(open.read_only.as_ref().is_some_and(|r| !r.on));
        // Read only switched elsewhere (another window, or the owner's other device): the next drawing has it.
        core.sync_share_set_read_only(form.clone(), true).unwrap();
        assert!(draw(&form).read_only.is_some_and(|r| r.on), "the refresh missed Read only");
        // The folder removed: the same form now draws it gone, and stops asking.
        core.sync_remove(f.id.clone(), false).unwrap();
        let gone = draw(&form);
        assert!(gone.gone.is_some() && gone.copy_title.is_none() && gone.refresh_ms.is_none(), "{gone:?}");
        let new = core.sync_share_window(core.sync_share_form_new("/tmp/seedwell-mock/Novel".into()), Vec::new(), NOW_S, Arc::new(Clock));
        assert_eq!(new.refresh_ms, None, "nothing elsewhere changes a folder not synced yet");
    }
}
