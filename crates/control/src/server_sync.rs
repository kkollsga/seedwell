//! The `sync.*` methods (SPEC 7G.5, 9A): folder sync through the control socket. A child of `server` so it
//! shares the permission gate, the activity log and the two-step removal.
//!
//! Permissions (SPEC 9A.3): read-only for `sync.list`, `sync.status`, `sync.members`, `sync.conflicts` and
//! `sync.settings`; full for `sync.share` and `sync.invites` (they return invitation links), for changing roles
//! and removing devices (`sync.set_role`, `sync.remove_device`), for what moves files on disk (`sync.rename`,
//! `sync.move`, removal with `delete_files`) and for changing settings (`sync.set_settings`); control for the
//! rest (`sync.resolve` moves a file too, but only into the folder's archive). Links are never written to the
//! activity log: [`crate::activity::describe`] leaves them out and every entry is scrubbed again.
//!
//! A channel closed to agents (SPEC §7G.7) does not show here either: not as a folder's `channel_id`, not in a link
//! of `sync.invites` (a folder's link can carry its channel's invitation, and with it the channel), not as a
//! connected device known only through it (or through private messages while those are closed), and `--chat` names
//! only channels open to agents.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use trs_platform::fs::Trashed;
use trs_sync::ondemand::Deleted;
use trs_sync_net::{Keep, SyncHandle};
use trs_sync_net::status::FolderStatus;

use super::{Conn, Ctx, Reply, blocking, denied, reply};
use crate::backend::Backend;
use crate::confirm::{SyncPlan, TokenError};
use crate::perm::Permission;
use crate::proto::*;
use crate::sync::{self as s, folder_dto, folder_id, folder_path, resolve_folder};
use crate::types::*;

fn pick(h: &SyncHandle, folder: &str) -> Result<FolderStatus, ApiError> {
    let snap = h.snapshot();
    resolve_folder(&snap.folders, folder).cloned()
}

fn status_of(h: &SyncHandle, id: &str) -> Result<FolderStatus, ApiError> {
    h.snapshot().folders.into_iter().find(|f| f.id == id).ok_or_else(|| ApiError::not_found("the folder is no longer synced"))
}

/// A folder for an agent: the channel that goes with it only while that channel is open to agents.
fn folder_out(h: &SyncHandle, f: &FolderStatus) -> SyncFolderDto {
    let mut d = folder_dto(f);
    if d.channel_id.as_deref().is_some_and(|c| !super::chat_methods::channel_visible(h, c)) {
        d.channel_id = None;
    }
    d
}

/// `sync.list` for an agent: no channel closed to agents, and no device this Mac is connected to only for such a
/// channel or for private messages while those are closed (one that shares a folder with it stays).
fn list_out(h: &SyncHandle, snap: &trs_sync_net::status::Snapshot, external: Option<u16>) -> SyncListDto {
    let mut l = s::list_dto(snap, external);
    l.folders = snap.folders.iter().map(|f| folder_out(h, f)).collect();
    let mut known: BTreeSet<String> = BTreeSet::new();
    for c in super::chat_methods::visible_channels(h) {
        if let Ok(id) = c.id.parse() {
            known.extend(h.members(&id).unwrap_or_default().into_iter().map(|m| m.device.to_string()));
        }
    }
    if super::chat_methods::directs_visible(h) {
        known.extend(h.chat_directs().into_iter().map(|d| d.device.to_string()));
    }
    l.devices.retain(|d| !d.folders.is_empty() || known.contains(&d.id));
    l
}

/// A folder in the answer, with what happened for the log.
fn folder_reply(h: &SyncHandle, f: &FolderStatus, what: &str) -> Result<Reply, ApiError> {
    let d = folder_out(h, f);
    let mut r = reply(&d)?;
    r.note = Some(format!("{what} {} ({})", clip(&d.name, 80), short(&d.id)));
    Ok(r)
}

fn need_full(ctx_level: Permission, what: &str) -> Result<(), ApiError> {
    if ctx_level.allows(Permission::Full) { Ok(()) } else { Err(denied(what, Permission::Full, ctx_level)) }
}

/// `host:port` or `ip:port`, resolved (the first address).
pub(super) async fn address(raw: &str) -> Result<SocketAddr, ApiError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 300 || raw.chars().any(char::is_control) {
        return Err(ApiError::params("`address` is host:port"));
    }
    if let Ok(a) = raw.parse::<SocketAddr>() {
        return Ok(a);
    }
    let bad = || ApiError::invalid(format!("`{}` is not host:port, or the host is unknown", clip(raw, 100)));
    if !raw.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok_and(|p| p != 0)) {
        return Err(bad());
    }
    let mut it = tokio::net::lookup_host(raw.to_owned()).await.map_err(|_| bad())?;
    it.next().ok_or_else(bad)
}

/// Run one `sync.*` request.
pub(super) async fn call<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, req: Request) -> Result<Reply, ApiError> {
    let h = ctx.backend.sync().ok_or_else(s::not_running)?;
    match req {
        Request::SyncList(_) => {
            let snap = h.snapshot();
            let external = ctx.backend.reachability().await.and_then(|r| r.sync_external_port);
            reply(&list_out(&h, &snap, external))
        }
        Request::SyncStatus(p) => reply(&folder_out(&h, &pick(&h, &p.folder)?)),
        Request::SyncAdd(p) => {
            let path = folder_path(&p.path, "path")?;
            let f = h.add_folder(path).await.map_err(s::err)?;
            folder_reply(&h, &f, "shared")
        }
        Request::SyncJoin(p) => {
            let path = match p.path.as_deref() {
                Some(raw) => folder_path(raw, "path")?,
                None => s::default_join_path(&h.settings().default_location, &p.key_or_link)?,
            };
            let f = if p.on_demand {
                h.join_folder_on_demand(&p.key_or_link, path).await
            } else {
                h.join_folder(&p.key_or_link, path).await
            }
            .map_err(s::err)?;
            folder_reply(&h, &f, if p.on_demand { "joined (on demand)" } else { "joined" })
        }
        Request::SyncShare(p) => {
            let f = pick(&h, &p.folder)?;
            let role = s::parse_role(p.role.as_deref().unwrap_or("read_write"))?;
            let (uses, days) = (p.uses.unwrap_or(1), p.days.unwrap_or(7));
            let inv = match p.chat.as_deref().map(str::trim) {
                None | Some("") => h.create_invite(&folder_id(&f)?, role, uses, days).map_err(s::err)?,
                Some(q) => {
                    let channel = if q.eq_ignore_ascii_case("new") {
                        // An agent's channel: an agent channel, open to agents on every member device.
                        let c = h.chat_new_for_agents(&f.name).await.map_err(s::chat_err)?;
                        c.id.parse().map_err(|_| ApiError::internal("the service returned a malformed channel id"))?
                    } else {
                        super::chat_methods::pick(&h, q)?.1
                    };
                    h.create_bundle_invite(&folder_id(&f)?, role, uses, days, &channel).map_err(s::err)?
                }
            };
            let d = folder_dto(&f);
            let mut r = reply(&s::invite_dto(&inv))?;
            // What was handed out, never the link itself.
            let uses = if inv.max_uses == 0 { "unlimited uses".to_string() } else { format!("{} use(s)", inv.max_uses) };
            let with = inv.channel_name.as_deref().map(|c| format!(" and channel {}", clip(c, 80))).unwrap_or_default();
            r.note = Some(format!("a {} link ({uses}) to {} ({}){with} made", s::role_name(role), clip(&d.name, 80), short(&d.id)));
            Ok(r)
        }
        Request::SyncMembers(p) => {
            let f = pick(&h, &p.folder)?;
            let members = h.members(&folder_id(&f)?).map_err(s::err)?;
            let d = folder_dto(&f);
            reply(&SyncMembersDto { id: d.id, name: d.name, members: members.iter().map(s::member_dto).collect() })
        }
        Request::SyncInvites(p) => {
            let f = pick(&h, &p.folder)?;
            let invites = h.invites(&folder_id(&f)?).map_err(s::err)?;
            let d = folder_dto(&f);
            // A link that also carries a channel closed to agents is left out: it would hand over that channel.
            let open = |i: &&trs_sync_net::InviteInfo| i.channel_id.as_deref().is_none_or(|c| super::chat_methods::channel_visible(&h, c));
            let invites = invites.iter().filter(open).take(1000).map(s::invite_dto).collect();
            let mut r = reply(&SyncInvitesDto { id: d.id.clone(), name: d.name.clone(), invites })?;
            r.note = Some(format!("the links of {} ({}) shown", clip(&d.name, 80), short(&d.id)));
            Ok(r)
        }
        Request::SyncRevokeInvite(p) => {
            let f = pick(&h, &p.folder)?;
            h.revoke_invite(&folder_id(&f)?, &p.invite).map_err(s::err)?;
            folder_reply(&h, &status_of(&h, &f.id)?, "a link revoked in")
        }
        Request::SyncSetRole(p) => {
            let f = pick(&h, &p.folder)?;
            let id = folder_id(&f)?;
            let role = s::parse_role(&p.role)?;
            let members = h.members(&id).map_err(s::err)?;
            let m = s::resolve_member(&members, &p.device)?;
            h.set_role(&id, &m.device, role).map_err(s::err)?;
            let mut r = folder_reply(&h, &status_of(&h, &f.id)?, "role changed in")?;
            r.note = Some(format!("{} is now {} in {}", clip(&m.name, 60), s::role_name(role), clip(&f.name, 80)));
            Ok(r)
        }
        Request::SyncRemoveDevice(p) => {
            let f = pick(&h, &p.folder)?;
            let id = folder_id(&f)?;
            let members = h.members(&id).map_err(s::err)?;
            let m = s::resolve_member(&members, &p.device)?;
            h.remove_device(&id, &m.device).map_err(s::err)?;
            let mut r = folder_reply(&h, &status_of(&h, &f.id)?, "device removed from")?;
            r.note = Some(format!("{} removed from {}", clip(&m.name, 60), clip(&f.name, 80)));
            Ok(r)
        }
        Request::SyncSetShareUntil(p) => {
            let f = pick(&h, &p.folder)?;
            let id = folder_id(&f)?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let span = u64::from(p.days) * 86_400;
            let until = match (p.days, p.extend, f.share_until) {
                (0, _, _) => 0,
                (_, true, Some(end)) => end.max(now) + span,
                (_, true, None) => return Err(ApiError::invalid("the folder is shared with no end; give the days from now instead")),
                (_, false, _) => now + span,
            };
            h.set_share_until(&id, until).map_err(s::err)?;
            let st = status_of(&h, &f.id)?;
            let mut r = folder_reply(&h, &st, "sharing changed for")?;
            r.note = Some(match st.share_until {
                Some(t) => format!("{} is shared until {}", clip(&f.name, 80), crate::human::date(t)),
                None => format!("{} is shared with no end", clip(&f.name, 80)),
            });
            Ok(r)
        }
        Request::SyncRename(p) => {
            let f = pick(&h, &p.folder)?;
            let f = h.rename_folder(&folder_id(&f)?, &p.name).await.map_err(s::err)?;
            folder_reply(&h, &f, "renamed to")
        }
        Request::SyncMove(p) => {
            let f = pick(&h, &p.folder)?;
            let path = folder_path(&p.path, "path")?;
            let f = h.move_folder(&folder_id(&f)?, path).await.map_err(s::err)?;
            let mut r = folder_reply(&h, &f, "moved")?;
            r.note = Some(format!("moved {} to {}", clip(&f.name, 80), clip(&f.path, 200)));
            Ok(r)
        }
        Request::SyncPause(p) => pause(&h, &p.folder, true).await,
        Request::SyncResume(p) => pause(&h, &p.folder, false).await,
        Request::SyncRescan(p) => {
            let f = pick(&h, &p.folder)?;
            h.rescan(&folder_id(&f)?).map_err(s::err)?;
            folder_reply(&h, &status_of(&h, &f.id)?, "rescan of")
        }
        Request::SyncSetLanOnly(p) => {
            let f = pick(&h, &p.folder)?;
            h.set_lan_only(&folder_id(&f)?, p.lan_only).map_err(s::err)?;
            folder_reply(&h, &status_of(&h, &f.id)?, if p.lan_only { "local network only for" } else { "everywhere again for" })
        }
        Request::SyncConflicts(p) => {
            let f = pick(&h, &p.folder)?;
            let found = h.conflicts(&folder_id(&f)?).await.map_err(s::err)?;
            let d = folder_dto(&f);
            let conflicts = found.into_iter().take(1000).map(|(copy, original)| s::conflict_dto(&copy, &original)).collect();
            reply(&SyncConflictsDto { id: d.id, name: d.name, conflicts })
        }
        Request::SyncResolve(p) => {
            let keep = match p.keep.as_str() {
                "original" => Keep::Original,
                "copy" => Keep::Copy,
                "both" => Keep::Both,
                _ => return Err(ApiError::params("`keep` is `original`, `copy` or `both`")),
            };
            let f = pick(&h, &p.folder)?;
            h.resolve_conflict(&folder_id(&f)?, &p.copy, keep).await.map_err(s::err)?;
            folder_reply(&h, &status_of(&h, &f.id)?, &format!("resolved a conflict (kept {}) in", p.keep))
        }
        Request::SyncAddAddress(p) => {
            let f = pick(&h, &p.folder)?;
            let a = address(&p.address).await?;
            h.add_address(&folder_id(&f)?, a).map_err(s::err)?;
            let mut r = folder_reply(&h, &status_of(&h, &f.id)?, "address added to")?;
            r.note = Some(format!("{a} for {} ({})", clip(&f.name, 80), short(&f.id)));
            Ok(r)
        }
        Request::SyncSetMode(p) => {
            let on = match p.mode.as_str() {
                "on_demand" | "on-demand" => true,
                "full" => false,
                _ => return Err(ApiError::params("`mode` is `full` or `on_demand`")),
            };
            let f = pick(&h, &p.folder)?;
            let place = p.path.as_deref().map(|raw| folder_path(raw, "path")).transpose()?;
            let f = h.set_on_demand(&folder_id(&f)?, on, place).await.map_err(s::err)?;
            folder_reply(&h, &f, if on { "on demand:" } else { "full:" })
        }
        r @ (Request::SyncItems(_)
        | Request::SyncChanges(_)
        | Request::SyncFetch(_)
        | Request::SyncCancelFetch(_)
        | Request::SyncPin(_)
        | Request::SyncUnpin(_)
        | Request::SyncEvict(_)
        | Request::SyncHostWrite(_)
        | Request::SyncHostMkdir(_)
        | Request::SyncHostDelete(_)
        | Request::SyncHostRename(_)) => on_demand_call(&h, r).await,
        Request::SyncUnavailable(p) => {
            let folders = match &p.folder {
                Some(f) => vec![pick(&h, f)?],
                None => h.snapshot().folders,
            };
            let mut out = SyncUnavailableDto::default();
            for f in folders {
                // (A folder not running, held back by an unfinished move, has none to list.)
                let Ok(files) = h.unavailable(&folder_id(&f)?) else { continue };
                if !files.is_empty() {
                    let files = files.into_iter().map(|(path, size)| SyncUnavailableFileDto { path, size }).collect();
                    out.folders.push(SyncUnavailableFolderDto { id: f.id.clone(), name: f.name.clone(), files });
                }
            }
            reply(&out)
        }
        Request::SyncRemove(p) => remove_plan(ctx, conn, &h, p).await,
        Request::SyncRemoveConfirm(p) => remove_confirm(ctx, conn, &h, p).await,
        Request::SyncSettings(_) => reply(&settings_dto(&h.settings(), Vec::new(), String::new())),
        Request::SyncSetSettings(p) => {
            // The port first: sync starts again on it, and the rest goes to the new service.
            let h = match p.port {
                Some(port) if port != 0 && port < 1024 => return Err(ApiError::params("`port` is 0 (choose one) or 1024-65535")),
                Some(port) => {
                    ctx.backend.sync_move_port(port).await.map_err(|e| match e {
                        s::PortChange::Invalid => ApiError::params("`port` is 0 (choose one) or 1024-65535"),
                        s::PortChange::NotRunning => s::not_running(),
                        e => ApiError::new(ErrorCode::Engine, e.to_string()),
                    })?;
                    ctx.backend.sync().ok_or_else(s::not_running)?
                }
                None => h,
            };
            set_settings(&h, p)
        }
        _ => Err(ApiError::internal("not a sync method")),
    }
}

async fn pause(h: &SyncHandle, folder: &str, paused: bool) -> Result<Reply, ApiError> {
    let f = pick(h, folder)?;
    h.set_paused(&folder_id(&f)?, paused).await.map_err(s::err)?;
    folder_reply(h, &status_of(h, &f.id)?, if paused { "paused" } else { "resumed" })
}

/// Step 1: what will happen, and a token. With `delete_files` the Trash is checked now (same volume, or a Recycle
/// Bin that keeps it), so the caller learns before confirming that it cannot be done.
async fn remove_plan<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, h: &SyncHandle, p: SyncRemoveParams) -> Result<Reply, ApiError> {
    let level = ctx.perm.get();
    if p.delete_files {
        need_full(level, "sync.remove with delete_files")?;
    }
    let f = pick(h, &p.folder)?;
    let trash_to = if p.delete_files {
        let (trash, folder) = (ctx.backend.trash_dir().await, files_of(h, &f));
        // Off the async threads: the Recycle Bin's check adds up a folder's size.
        let t = blocking(move || s::trash_target(&folder, trash.as_deref())).await??;
        // An on-demand folder's cache goes under the folder's name.
        Some(match (f.on_demand, t, Path::new(&f.path).file_name()) {
            (true, Trashed::At(p), Some(name)) => Trashed::At(p.with_file_name(name)),
            (_, t, _) => t,
        })
    } else {
        None
    };
    let client = conn.client.clone().unwrap_or_default();
    let token = {
        let mut c = ctx.sync_confirms.lock().unwrap_or_else(|p| p.into_inner());
        c.issue(Instant::now(), SyncPlan { folder: f.id.clone(), delete_files: p.delete_files, client })
    };
    let ttl = ctx.opts.confirm_ttl.as_secs();
    let d = folder_out(h, &f);
    // Off the async threads: it reads the folder's whole index.
    let only_here = match folder_id(&f) {
        Ok(id) => {
            let h = h.clone();
            blocking(move || h.only_here(&id)).await?.unwrap_or(0)
        }
        Err(_) => 0,
    };
    let others = s::others_after_removal(only_here);
    let summary = match &trash_to {
        Some(t) => format!(
            concat!("Will stop syncing `{}` on this ", trs_platform::computer!(), " AND MOVE THE FOLDER {} TO THE {}{}. {others} Confirmation is valid for {ttl} s."),
            d.name,
            d.path,
            trs_platform::TRASH.to_uppercase(),
            s::trash_shown(t),
            others = others,
            ttl = ttl
        ),
        None => format!(
            concat!("Will stop syncing `{}` on this ", trs_platform::computer!(), "; its files stay in {}. {others} Confirmation is valid for {ttl} s."),
            d.name,
            d.path,
            others = others,
            ttl = ttl
        ),
    };
    let mut r = reply(&SyncRemovePlanDto { token, expires_in_secs: ttl, delete_files: p.delete_files, folder: d.clone(), only_here, summary })?;
    r.note = Some(format!("plan for {} ({}), delete_files={}", clip(&d.name, 80), short(&d.id), p.delete_files));
    Ok(r)
}

/// Where a folder's files are on this device: an on-demand folder's are in its cache (removing it moves them to its
/// place, from where they go to the Trash), a full folder's at its place.
fn files_of(h: &SyncHandle, f: &FolderStatus) -> PathBuf {
    folder_id(f).ok().and_then(|id| h.folder_files(&id)).unwrap_or_else(|| PathBuf::from(&f.path))
}

/// Step 2: stop syncing, then (with `delete_files`) move the folder to the Trash.
async fn remove_confirm<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, h: &SyncHandle, p: RemoveConfirmParams) -> Result<Reply, ApiError> {
    let client = conn.client.clone().unwrap_or_default();
    let plan = {
        let mut c = ctx.sync_confirms.lock().unwrap_or_else(|p| p.into_inner());
        c.redeem(Instant::now(), p.token.trim(), &client)
    };
    let plan = plan.map_err(|e| match e {
        TokenError::Unknown => ApiError::new(
            ErrorCode::InvalidToken,
            "unknown confirmation token (wrong, already used, or issued to another client); run `seedwell sync remove` again",
        ),
        TokenError::Expired => ApiError::new(
            ErrorCode::TokenExpired,
            format!("the confirmation expired after {} s; run `seedwell sync remove` again", ctx.opts.confirm_ttl.as_secs()),
        ),
    })?;
    // The level may have dropped since step 1.
    if plan.delete_files {
        need_full(ctx.perm.get(), "sync.remove_confirm with delete_files")?;
    }
    let f = status_of(h, &plan.folder)?;
    let id = folder_id(&f)?;
    // Checked again (a disk may have gone) before anything is removed.
    let trash = if plan.delete_files {
        let (trash, folder) = (ctx.backend.trash_dir().await, files_of(h, &f));
        let dir = trash.clone();
        blocking(move || s::trash_target(&folder, dir.as_deref())).await??;
        Some(trash)
    } else {
        None
    };
    let d = folder_dto(&f);
    let mut out = SyncRemovedDto { id: d.id.clone(), name: d.name.clone(), path: d.path.clone(), files: "kept".into(), trashed_to: None };
    if let Some(trash) = trash {
        // To the Trash from where the files are (an on-demand folder's from its cache, checked above), so the check
        // holds; refused, they stay as an ordinary folder at the folder's place.
        let files = h.remove_folder_for_trash(&id).await.map_err(s::err)?;
        let from = files.clone();
        let t = blocking(move || s::move_to_trash(&from, trash.as_deref())).await?;
        let place = PathBuf::from(&f.path);
        let at = h.finish_removal(&files, &place, t.is_ok()).await.map_err(s::err)?;
        let t = t.map_err(|e| {
            ApiError::new(
                ErrorCode::Engine,
                format!(
                    concat!("the folder no longer syncs, but moving it to the ", trs_platform::trash!(), " failed ({}); its files are still in {}"),
                    e,
                    at.display()
                ),
            )
        })?;
        out.files = "trashed".into();
        out.trashed_to = t.path().map(|p| p.display().to_string());
    } else {
        out.path = h.remove_folder(&id).await.map_err(s::err)?.display().to_string();
    }
    let mut r = reply(&out)?;
    r.note = Some(format!(
        "removed {} ({}){}",
        clip(&d.name, 80),
        short(&d.id),
        if out.files == "trashed" { concat!(" and moved it to the ", trs_platform::trash!()) } else { " (files kept)" }
    ));
    Ok(r)
}

fn settings_dto(st: &trs_sync_net::config::Settings, changed: Vec<String>, note: String) -> SyncSettingsDto {
    let lim = |kib: u32| (kib != 0).then(|| u64::from(kib) * 1024);
    SyncSettingsDto {
        device_name: st.device_name.clone(),
        user_name: st.user_name.clone(),
        port: st.port,
        lan_discovery: st.lan_discovery,
        dht_discovery: st.dht_discovery,
        relay_own_devices: st.relay_own_devices,
        relay_everyone: st.relay_everyone,
        up_limit: lim(st.up_kib),
        down_limit: lim(st.down_kib),
        default_location: st.default_location.clone(),
        changed,
        note,
    }
}

/// Change the device-wide settings and read them back.
fn set_settings(h: &SyncHandle, p: SyncSettingsParams) -> Result<Reply, ApiError> {
    if p.is_empty() {
        return Err(ApiError::params("give at least one setting to change (`seedwell sync settings` shows them)"));
    }
    let mut st = h.settings();
    let mut changed = Vec::new();
    let mut note = Vec::new();
    if let Some(n) = &p.device_name {
        let n: String = n.trim().chars().filter(|c| !c.is_control()).collect();
        if n.chars().count() > 64 {
            return Err(ApiError::params("`device_name` is at most 64 characters"));
        }
        st.device_name = n;
        changed.push("device_name");
    }
    if let Some(n) = &p.user_name {
        let n: String = n.trim().chars().filter(|c| !c.is_control()).collect();
        if n.chars().count() > 64 {
            return Err(ApiError::params("`user_name` is at most 64 characters"));
        }
        st.user_name = n;
        changed.push("user_name");
    }
    if p.port.is_some() {
        // Already moved (`call`): sync listens on it now.
        changed.push("port");
        note.push(format!("folder sync listens on port {} now", h.port()));
    }
    for (v, slot, name) in [
        (p.lan_discovery, &mut st.lan_discovery, "lan_discovery"),
        (p.dht_discovery, &mut st.dht_discovery, "dht_discovery"),
        (p.relay_own_devices, &mut st.relay_own_devices, "relay_own_devices"),
        (p.relay_everyone, &mut st.relay_everyone, "relay_everyone"),
    ] {
        if let Some(v) = v {
            *slot = v;
            changed.push(name);
        }
    }
    if p.lan_discovery.is_some() {
        note.push("LAN discovery follows at the next start".to_owned());
    }
    let kib = |b: u64, what: &str| -> Result<u32, ApiError> {
        if b > trs_state::config::MAX_RATE {
            return Err(ApiError::params(format!("`{what}` is above 1 TiB/s")));
        }
        Ok(u32::try_from(b.div_ceil(1024)).unwrap_or(u32::MAX))
    };
    if let Some(b) = p.up_limit {
        st.up_kib = kib(b, "up_limit")?;
        changed.push("up_limit");
    }
    if let Some(b) = p.down_limit {
        st.down_kib = kib(b, "down_limit")?;
        changed.push("down_limit");
    }
    if let Some(loc) = &p.default_location {
        st.default_location = if loc.trim().is_empty() { String::new() } else { folder_path(loc, "default_location")?.display().to_string() };
        changed.push("default_location");
    }
    h.set_settings(st).map_err(s::err)?;
    let changed: Vec<String> = changed.into_iter().map(str::to_owned).collect();
    let mut r = reply(&settings_dto(&h.settings(), changed.clone(), note.join("; ")))?;
    r.note = Some(changed.join(", "));
    Ok(r)
}


// ------------------------------------------------------------------ on demand (SPEC 7G.11)

fn kind_name(k: trs_sync_proto::model::FileKind) -> &'static str {
    match k {
        trs_sync_proto::model::FileKind::File => "file",
        trs_sync_proto::model::FileKind::Dir => "dir",
        trs_sync_proto::model::FileKind::Symlink => "symlink",
    }
}

fn item_state_name(s: trs_sync::ondemand::ItemState) -> &'static str {
    use trs_sync::ondemand::ItemState;
    match s {
        ItemState::OnlineOnly => "online_only",
        ItemState::Downloading => "downloading",
        ItemState::Downloaded => "downloaded",
        ItemState::ModifiedHere => "modified_here",
        ItemState::Unavailable => "unavailable",
    }
}

/// An item in protocol form.
pub(super) fn item_dto(i: &trs_sync::ondemand::Item) -> SyncItemDto {
    SyncItemDto {
        id: i.id.clone(),
        parent_id: i.parent_id.clone(),
        path: i.path.clone(),
        name: i.name.clone(),
        kind: kind_name(i.kind).into(),
        size: i.size,
        mtime_ns: i.mtime_ns,
        state: item_state_name(i.state).into(),
        pinned: i.pinned,
        content_version: i.content_version.clone(),
        metadata_version: i.metadata_version.clone(),
        exec: i.exec,
        symlink_target: (!i.symlink_target.is_empty()).then(|| i.symlink_target.clone()),
        conflict_of: i.conflict_of.clone(),
    }
}

/// The path of the item named by `item` (an identifier) or `path`; neither: the folder itself.
fn item_path(h: &SyncHandle, folder: &trs_sync_proto::ids::FolderId, item: &Option<String>, path: &Option<String>) -> Result<String, ApiError> {
    match (path, item) {
        (Some(p), _) => Ok(p.trim_matches('/').to_string()),
        (None, Some(id)) => h
            .od_item(folder, id)
            .map_err(s::err)?
            .map(|i| i.path)
            .ok_or_else(|| ApiError::not_found(format!("no item {} in the folder", clip(id, 80)))),
        (None, None) => Ok(String::new()),
    }
}

fn item_at(h: &SyncHandle, folder: &trs_sync_proto::ids::FolderId, path: &str) -> Result<SyncItemDto, ApiError> {
    h.od_item_at(folder, path)
        .map_err(s::err)?
        .map(|i| item_dto(&i))
        .ok_or_else(|| ApiError::not_found(format!("no item {} in the folder", clip(path, 200))))
}

async fn on_demand_call(h: &SyncHandle, r: Request) -> Result<Reply, ApiError> {
    match r {
        Request::SyncItems(p) => {
            let f = pick(h, &p.folder)?;
            let fid = folder_id(&f)?;
            let path = item_path(h, &fid, &p.item, &p.path)?;
            let item = item_at(h, &fid, &path)?;
            let (items, next) = if item.kind == "dir" {
                let limit = p.limit.unwrap_or(200).clamp(1, 1000) as usize;
                match h.od_children(&fid, &item.id, p.cursor.as_deref(), limit).map_err(s::err)? {
                    Some(page) => (page.items.iter().map(item_dto).collect(), page.next),
                    None => (Vec::new(), None),
                }
            } else {
                (Vec::new(), None)
            };
            reply(&SyncItemsDto { folder: f.id.clone(), item, items, next })
        }
        Request::SyncChanges(p) => {
            let f = pick(h, &p.folder)?;
            let limit = p.limit.unwrap_or(500).clamp(1, 1000) as usize;
            let page = h.od_changes(&folder_id(&f)?, p.anchor.unwrap_or(0), limit).map_err(s::err)?;
            reply(&SyncChangesDto { items: page.items.iter().map(item_dto).collect(), deleted: page.deleted, anchor: page.anchor, more: page.more })
        }
        Request::SyncFetch(p) => {
            let (f, fid, path) = target(h, &SyncItemParams { folder: p.folder.clone(), item: p.item.clone(), path: p.path.clone() })?;
            let wait = p.wait_s.unwrap_or(0).min(3600);
            let mut unavailable = Vec::new();
            if wait == 0 {
                h.od_request(&fid, &path).await.map_err(s::err)?;
            } else {
                match h.od_fetch(&fid, &path, std::time::Duration::from_secs(wait)).await {
                    Ok(done) => unavailable = done.unavailable,
                    // Still on its way: the answer says how far.
                    Err(trs_sync_net::SyncError::OnDemand(trs_sync_net::OnDemandError::NotHereYet, _)) => {}
                    Err(e) => return Err(s::err(e)),
                }
            }
            let item = item_at(h, &fid, &path)?;
            let pr = if item.kind == "file" { h.od_progress(&fid, &path).map_err(s::err)? } else { Default::default() };
            let here = item.kind != "file" || pr.here;
            let local_path = here.then(|| h.od_cache_path(&fid, &path).ok()).flatten().map(|p| p.display().to_string());
            let mut r = reply(&SyncFetchDto { item, local_path, done: pr.done, total: pr.total, unavailable })?;
            r.note = Some(format!("fetch {} in {} ({})", clip(&path, 200), clip(&f.name, 80), short(&f.id)));
            Ok(r)
        }
        Request::SyncCancelFetch(p) => {
            let (f, fid, path) = target(h, &p)?;
            h.od_cancel(&fid, &path).await.map_err(s::err)?;
            noted(&SyncHostDto { item: Some(item_at(h, &fid, &path)?), count: 0, ..Default::default() }, &format!("cancelled the fetch of {}", clip(&path, 200)), &f)
        }
        Request::SyncPin(p) => pin(h, &p, true).await,
        Request::SyncUnpin(p) => pin(h, &p, false).await,
        Request::SyncEvict(p) => {
            let (f, fid, path) = target(h, &p)?;
            let e = evict_dto(h.od_evict(&fid, &path).await.map_err(s::err)?);
            let mut r = reply(&e)?;
            r.note = Some(format!("removed the download of {} files in {} ({})", e.files, clip(&f.name, 80), short(&f.id)));
            Ok(r)
        }
        Request::SyncHostWrite(p) => {
            let f = pick(h, &p.folder)?;
            let fid = folder_id(&f)?;
            let from = std::path::PathBuf::from(&p.from);
            if !from.is_absolute() || !from.is_file() {
                return Err(ApiError::params("`from` is the absolute path of a file"));
            }
            let path = p.path.trim_matches('/').to_string();
            let at = h.od_write(&fid, &path, &from, p.base.as_deref()).await.map_err(s::err)?;
            let what = if at == path { format!("wrote {}", clip(&path, 200)) } else { format!("wrote {} as a conflict copy ({})", clip(&path, 200), clip(&at, 200)) };
            noted(&SyncHostDto { item: Some(item_at(h, &fid, &at)?), count: 1, ..Default::default() }, &what, &f)
        }
        Request::SyncHostMkdir(p) => {
            let (f, fid, path) = target(h, &p)?;
            h.od_mkdir(&fid, &path).await.map_err(s::err)?;
            noted(&SyncHostDto { item: Some(item_at(h, &fid, &path)?), count: 1, ..Default::default() }, &format!("made folder {}", clip(&path, 200)), &f)
        }
        Request::SyncHostDelete(p) => {
            let (f, fid, path) = target(h, &p)?;
            if path.is_empty() {
                return Err(ApiError::params("name an item (`item` or `path`): the folder itself is removed with `sync.remove`"));
            }
            match h.od_delete(&fid, &path).await.map_err(s::err)? {
                Deleted::Deleted { items, failed } => {
                    let dto = SyncHostDto { item: None, count: items, freed: None, failed: failed_dto(failed) };
                    noted(&dto, &format!("deleted {} ({items} items)", clip(&path, 200)), &f)
                }
                // A read-only member: the delete freed the content; the item stays, online only.
                Deleted::Freed(e) => {
                    let e = evict_dto(e);
                    let dto = SyncHostDto { item: Some(item_at(h, &fid, &path)?), count: 0, failed: e.failed.clone(), freed: Some(e) };
                    noted(&dto, &format!("freed {} (a read-only member deletes nothing)", clip(&path, 200)), &f)
                }
            }
        }
        Request::SyncHostRename(p) => {
            let f = pick(h, &p.folder)?;
            let fid = folder_id(&f)?;
            let (from, to) = (p.from.trim_matches('/').to_string(), p.to.trim_matches('/').to_string());
            let n = h.od_rename(&fid, &from, &to).await.map_err(s::err)?;
            noted(&SyncHostDto { item: Some(item_at(h, &fid, &to)?), count: n, ..Default::default() }, &format!("renamed {} to {}", clip(&from, 200), clip(&to, 200)), &f)
        }
        _ => Err(ApiError::internal("not an on-demand method")),
    }
}

fn failed_dto(failed: Vec<(String, String)>) -> Vec<SyncFailedDto> {
    failed.into_iter().map(|(path, error)| SyncFailedDto { path, error }).collect()
}

fn evict_dto(e: trs_sync::ondemand::Evicted) -> SyncEvictDto {
    SyncEvictDto { files: e.files, bytes: e.bytes, kept: e.kept, kept_why: failed_dto(e.kept_why), failed: failed_dto(e.failed) }
}

/// The folder and the path of the item `p` names.
fn target(h: &SyncHandle, p: &SyncItemParams) -> Result<(FolderStatus, trs_sync_proto::ids::FolderId, String), ApiError> {
    let f = pick(h, &p.folder)?;
    let fid = folder_id(&f)?;
    let path = item_path(h, &fid, &p.item, &p.path)?;
    Ok((f, fid, path))
}

/// The answer, with what happened for the log.
fn noted(dto: &SyncHostDto, what: &str, f: &FolderStatus) -> Result<Reply, ApiError> {
    let mut r = reply(dto)?;
    r.note = Some(format!("{what} in {} ({})", clip(&f.name, 80), short(&f.id)));
    Ok(r)
}

async fn pin(h: &SyncHandle, p: &SyncItemParams, on: bool) -> Result<Reply, ApiError> {
    let (f, fid, path) = target(h, p)?;
    h.od_pin(&fid, &path, on).await.map_err(s::err)?;
    let what = if on { "kept on this " } else { "no longer kept on this " };
    let shown = if path.is_empty() { "the whole folder".to_string() } else { clip(&path, 200) };
    noted(&SyncHostDto { item: Some(item_at(h, &fid, &path)?), count: 1, ..Default::default() }, &format!("{shown} {what}{}", trs_platform::computer!()), &f)
}
