//! The `sync.*` methods (SPEC 7G.5, 9A): folder sync through the control socket. A child of `server` so it
//! shares the permission gate, the activity log and the two-step removal.
//!
//! Permissions (SPEC 9A.3): read-only for `sync.list`, `sync.status`, `sync.members`, `sync.conflicts` and
//! `sync.settings`; full for `sync.share` and `sync.invites` (they return invitation links), for changing roles
//! and removing devices (`sync.set_role`, `sync.remove_device`), for what moves files on disk (`sync.rename`,
//! `sync.move`, removal with `delete_files`) and for changing settings (`sync.set_settings`); control for the
//! rest (`sync.resolve` moves a file too, but only into the folder's archive). Links are never written to the
//! activity log: [`crate::activity::describe`] leaves them out and every entry is scrubbed again.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

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

/// A folder in the answer, with what happened for the log.
fn folder_reply(f: &FolderStatus, what: &str) -> Result<Reply, ApiError> {
    let d = folder_dto(f);
    let mut r = reply(&d)?;
    r.note = Some(format!("{what} {} ({})", clip(&d.name, 80), short(&d.id)));
    Ok(r)
}

fn need_full(ctx_level: Permission, what: &str) -> Result<(), ApiError> {
    if ctx_level.allows(Permission::Full) { Ok(()) } else { Err(denied(what, Permission::Full, ctx_level)) }
}

/// `host:port` or `ip:port`, resolved (the first address).
async fn address(raw: &str) -> Result<SocketAddr, ApiError> {
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
            reply(&s::list_dto(&snap, external))
        }
        Request::SyncStatus(p) => reply(&folder_dto(&pick(&h, &p.folder)?)),
        Request::SyncAdd(p) => {
            let path = folder_path(&p.path, "path")?;
            let f = h.add_folder(path).await.map_err(s::err)?;
            folder_reply(&f, "shared")
        }
        Request::SyncJoin(p) => {
            let path = match p.path.as_deref() {
                Some(raw) => folder_path(raw, "path")?,
                None => s::default_join_path(&h.settings().default_location, &p.key_or_link)?,
            };
            let f = h.join_folder(&p.key_or_link, path).await.map_err(s::err)?;
            folder_reply(&f, "joined")
        }
        Request::SyncShare(p) => {
            let f = pick(&h, &p.folder)?;
            let role = s::parse_role(p.role.as_deref().unwrap_or("read_write"))?;
            let inv = h.create_invite(&folder_id(&f)?, role, p.uses.unwrap_or(1), p.days.unwrap_or(7)).map_err(s::err)?;
            let d = folder_dto(&f);
            let mut r = reply(&s::invite_dto(&inv))?;
            // What was handed out, never the link itself.
            let uses = if inv.max_uses == 0 { "unlimited uses".to_string() } else { format!("{} use(s)", inv.max_uses) };
            r.note = Some(format!("a {} link ({uses}) to {} ({}) made", s::role_name(role), clip(&d.name, 80), short(&d.id)));
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
            let mut r = reply(&SyncInvitesDto { id: d.id.clone(), name: d.name.clone(), invites: invites.iter().take(1000).map(s::invite_dto).collect() })?;
            r.note = Some(format!("the links of {} ({}) shown", clip(&d.name, 80), short(&d.id)));
            Ok(r)
        }
        Request::SyncRevokeInvite(p) => {
            let f = pick(&h, &p.folder)?;
            h.revoke_invite(&folder_id(&f)?, &p.invite).map_err(s::err)?;
            folder_reply(&status_of(&h, &f.id)?, "a link revoked in")
        }
        Request::SyncSetRole(p) => {
            let f = pick(&h, &p.folder)?;
            let id = folder_id(&f)?;
            let role = s::parse_role(&p.role)?;
            let members = h.members(&id).map_err(s::err)?;
            let m = s::resolve_member(&members, &p.device)?;
            h.set_role(&id, &m.device, role).map_err(s::err)?;
            let mut r = folder_reply(&status_of(&h, &f.id)?, "role changed in")?;
            r.note = Some(format!("{} is now {} in {}", clip(&m.name, 60), s::role_name(role), clip(&f.name, 80)));
            Ok(r)
        }
        Request::SyncRemoveDevice(p) => {
            let f = pick(&h, &p.folder)?;
            let id = folder_id(&f)?;
            let members = h.members(&id).map_err(s::err)?;
            let m = s::resolve_member(&members, &p.device)?;
            h.remove_device(&id, &m.device).map_err(s::err)?;
            let mut r = folder_reply(&status_of(&h, &f.id)?, "device removed from")?;
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
            let mut r = folder_reply(&st, "sharing changed for")?;
            r.note = Some(match st.share_until {
                Some(t) => format!("{} is shared until {}", clip(&f.name, 80), crate::human::date(t)),
                None => format!("{} is shared with no end", clip(&f.name, 80)),
            });
            Ok(r)
        }
        Request::SyncRename(p) => {
            let f = pick(&h, &p.folder)?;
            let f = h.rename_folder(&folder_id(&f)?, &p.name).await.map_err(s::err)?;
            folder_reply(&f, "renamed to")
        }
        Request::SyncMove(p) => {
            let f = pick(&h, &p.folder)?;
            let path = folder_path(&p.path, "path")?;
            let f = h.move_folder(&folder_id(&f)?, path).await.map_err(s::err)?;
            let mut r = folder_reply(&f, "moved")?;
            r.note = Some(format!("moved {} to {}", clip(&f.name, 80), clip(&f.path, 200)));
            Ok(r)
        }
        Request::SyncPause(p) => pause(&h, &p.folder, true).await,
        Request::SyncResume(p) => pause(&h, &p.folder, false).await,
        Request::SyncRescan(p) => {
            let f = pick(&h, &p.folder)?;
            h.rescan(&folder_id(&f)?).map_err(s::err)?;
            folder_reply(&status_of(&h, &f.id)?, "rescan of")
        }
        Request::SyncSetLanOnly(p) => {
            let f = pick(&h, &p.folder)?;
            h.set_lan_only(&folder_id(&f)?, p.lan_only).map_err(s::err)?;
            folder_reply(&status_of(&h, &f.id)?, if p.lan_only { "local network only for" } else { "everywhere again for" })
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
            folder_reply(&status_of(&h, &f.id)?, &format!("resolved a conflict (kept {}) in", p.keep))
        }
        Request::SyncAddAddress(p) => {
            let f = pick(&h, &p.folder)?;
            let a = address(&p.address).await?;
            h.add_address(&folder_id(&f)?, a).map_err(s::err)?;
            let mut r = folder_reply(&status_of(&h, &f.id)?, "address added to")?;
            r.note = Some(format!("{a} for {} ({})", clip(&f.name, 80), short(&f.id)));
            Ok(r)
        }
        Request::SyncRemove(p) => remove_plan(ctx, conn, &h, p).await,
        Request::SyncRemoveConfirm(p) => remove_confirm(ctx, conn, &h, p).await,
        Request::SyncSettings(_) => reply(&settings_dto(&h.settings(), Vec::new(), String::new())),
        Request::SyncSetSettings(p) => set_settings(&h, p),
        _ => Err(ApiError::internal("not a sync method")),
    }
}

async fn pause(h: &SyncHandle, folder: &str, paused: bool) -> Result<Reply, ApiError> {
    let f = pick(h, folder)?;
    h.set_paused(&folder_id(&f)?, paused).await.map_err(s::err)?;
    folder_reply(&status_of(h, &f.id)?, if paused { "paused" } else { "resumed" })
}

/// Step 1: what will happen, and a token. With `delete_files` the Trash is checked now (same volume), so the
/// caller learns before confirming that it cannot be done.
async fn remove_plan<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, h: &SyncHandle, p: SyncRemoveParams) -> Result<Reply, ApiError> {
    let level = ctx.perm.get();
    if p.delete_files {
        need_full(level, "sync.remove with delete_files")?;
    }
    let f = pick(h, &p.folder)?;
    let trash_to = if p.delete_files {
        let trash = ctx.backend.trash_dir().await;
        Some(s::trash_target(Path::new(&f.path), trash.as_deref())?)
    } else {
        None
    };
    let client = conn.client.clone().unwrap_or_default();
    let token = {
        let mut c = ctx.sync_confirms.lock().unwrap_or_else(|p| p.into_inner());
        c.issue(Instant::now(), SyncPlan { folder: f.id.clone(), delete_files: p.delete_files, client })
    };
    let ttl = ctx.opts.confirm_ttl.as_secs();
    let d = folder_dto(&f);
    let summary = match &trash_to {
        Some(t) => format!(
            "Will stop syncing `{}` on this Mac AND MOVE THE FOLDER {} TO THE TRASH ({}). Other devices keep their copies. Confirmation is valid for {ttl} s.",
            d.name,
            d.path,
            t.display()
        ),
        None => format!("Will stop syncing `{}` on this Mac; its files stay in {}. Other devices keep their copies. Confirmation is valid for {ttl} s.", d.name, d.path),
    };
    let mut r = reply(&SyncRemovePlanDto { token, expires_in_secs: ttl, delete_files: p.delete_files, folder: d.clone(), summary })?;
    r.note = Some(format!("plan for {} ({}), delete_files={}", clip(&d.name, 80), short(&d.id), p.delete_files));
    Ok(r)
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
            "unknown confirmation token (wrong, already used, or issued to another client); call `sync.remove` again",
        ),
        TokenError::Expired => ApiError::new(
            ErrorCode::TokenExpired,
            format!("the confirmation expired after {} s; call `sync.remove` again", ctx.opts.confirm_ttl.as_secs()),
        ),
    })?;
    // The level may have dropped since step 1.
    if plan.delete_files {
        need_full(ctx.perm.get(), "sync.remove_confirm with delete_files")?;
    }
    let f = status_of(h, &plan.folder)?;
    let id = folder_id(&f)?;
    // Checked again (a disk may have gone) before anything is removed.
    let target = if plan.delete_files {
        let trash = ctx.backend.trash_dir().await;
        Some(s::trash_target(Path::new(&f.path), trash.as_deref())?)
    } else {
        None
    };
    let path = h.remove_folder(&id).await.map_err(s::err)?;
    let d = folder_dto(&f);
    let mut out = SyncRemovedDto { id: d.id.clone(), name: d.name.clone(), path: path.display().to_string(), files: "kept".into(), trashed_to: None };
    if let Some(t) = target {
        let (from, to) = (path.clone(), t.clone());
        blocking(move || s::move_to_trash(&from, &to)).await?.map_err(|e| {
            ApiError::new(
                ErrorCode::Engine,
                format!("the folder no longer syncs, but moving it to the Trash failed ({e}); its files are still in {}", path.display()),
            )
        })?;
        out.files = "trashed".into();
        out.trashed_to = Some(t.display().to_string());
    }
    let mut r = reply(&out)?;
    r.note = Some(format!(
        "removed {} ({}){}",
        clip(&d.name, 80),
        short(&d.id),
        if out.files == "trashed" { " and moved it to the Trash" } else { " (files kept)" }
    ));
    Ok(r)
}

fn settings_dto(st: &trs_sync_net::config::Settings, changed: Vec<String>, note: String) -> SyncSettingsDto {
    let lim = |kib: u32| (kib != 0).then(|| u64::from(kib) * 1024);
    SyncSettingsDto {
        device_name: st.device_name.clone(),
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
        return Err(ApiError::params("give at least one setting to change (`sync.settings` reads them)"));
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
    if let Some(port) = p.port {
        if port != 0 && port < 1024 {
            return Err(ApiError::params("`port` is 0 (choose one) or 1024-65535"));
        }
        st.port = port;
        changed.push("port");
        note.push("the port applies at the next start");
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
        note.push("LAN discovery follows at the next start");
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
