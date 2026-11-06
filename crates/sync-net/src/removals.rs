//! An admin's removal of a user (SPEC §7G.1 *Removing a device*). Only owners sign membership changes, so an admin
//! asks: its device signs a [`RemoveRequest`], sends it to the folder's owner devices it can reach, and cuts the user
//! off at once (no session with it in that folder) until the owners' removal arrives. It keeps the request, on disk
//! next to the membership, and sends it again to every owner device it meets until the removal is in the membership
//! or an owner device refuses it (the reason is shown). An owner device that receives one carries it out by itself,
//! with no question to its user, when the sender is an admin of the folder now and the device a user there (not an
//! owner or an admin, nor one of the owner's own devices): an ordinary owner removal, so demoting the admin later
//! changes nothing.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use trs_sync_proto::members::{Body, Role};
use trs_sync_proto::msg::{Close, MAX_REMOVE_REASON, Message, RemoveReply, RemoveRequest};

use crate::shared::{Ctx, PeerHandle, Space, SpaceKind, lock};

/// A request this device made, as it keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The request, signed.
    pub request: RemoveRequest,
    /// An owner device refused it, and why (`None`: waiting for an owner device).
    pub refused: Option<String>,
}

/// A removal this device asked for, as the member list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asked {
    /// Waiting for an owner's device (the device is cut off here meanwhile).
    Waiting,
    /// An owner's device refused it, and why.
    Refused(String),
}

/// The file's form of a [`Pending`].
#[derive(Serialize, Deserialize)]
struct Kept {
    device: Vec<u8>,
    #[serde(default)]
    seen: u64,
    at_s: u64,
    by: Vec<u8>,
    sig: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refused: Option<String>,
}

/// Where `f`'s requests are kept: next to its membership.
fn path(f: &Space) -> PathBuf {
    f.members_path.with_extension("removals")
}

/// The requests kept for `f` (none when there is no file, or it cannot be read).
pub(crate) fn load(f: &Space) -> Vec<Pending> {
    let Ok(bytes) = std::fs::read(path(f)) else { return Vec::new() };
    let kept: Vec<Kept> = serde_json::from_slice(&bytes).unwrap_or_default();
    kept.into_iter()
        .filter_map(|k| {
            let request = RemoveRequest { folder_id: f.id, device: k.device.try_into().ok()?, seen: k.seen, at_s: k.at_s, by: k.by.try_into().ok()?, sig: k.sig.try_into().ok()? };
            Some(Pending { request, refused: k.refused })
        })
        .collect()
}

fn save(f: &Space, all: &[Pending]) {
    let p = path(f);
    let result = if all.is_empty() {
        match std::fs::remove_file(&p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        let kept: Vec<Kept> = all
            .iter()
            .map(|x| Kept { device: x.request.device.to_vec(), seen: x.request.seen, at_s: x.request.at_s, by: x.request.by.to_vec(), sig: x.request.sig.to_vec(), refused: x.refused.clone() })
            .collect();
        serde_json::to_vec(&kept).map_err(io::Error::other).and_then(|b| crate::roles::write_bytes(&p, &b))
    };
    if let Err(e) = result {
        f.problem(format!("the removals asked of the {}'s owner could not be saved: {e}", crate::roles::noun(f)));
    }
}

/// This device asked for `device`'s removal from `f` and no owner device refused it: it is cut off here meanwhile.
pub(crate) fn waiting(f: &Space, device: &[u8; 32]) -> bool {
    lock(&f.removals).iter().any(|x| x.request.device == *device && x.refused.is_none())
}

/// Asks the owners of `f` to remove `device` (this device is an admin there; the caller checked that `device` is a
/// user): the request is kept, so `device` is cut off here from now on ([`waiting`]); [`deliver`] sends it.
pub(crate) fn record(ctx: &Ctx, f: &Space, device: [u8; 32]) -> RemoveRequest {
    let req = RemoveRequest::new(&ctx.key, f.id, device, seen(f, &device), crate::roles::now_s());
    let mut all = lock(&f.removals);
    all.retain(|x| x.request.device != device);
    all.push(Pending { request: req.clone(), refused: None });
    save(f, &all);
    req
}

/// Sends `req` to the owner devices of `f` connected now, and ends the session of the device it names at once.
pub(crate) async fn deliver(ctx: &Arc<Ctx>, f: &Arc<Space>, req: RemoveRequest) {
    let device = req.device;
    for p in ctx.registry.peers() {
        if p.access(&f.id).is_none() {
            continue;
        }
        if p.device_pk == device {
            cut_off(f, &p).await;
        } else if f.role_of(&p.device_pk) == Some(Role::Owner) {
            let _ = p.control.send(Message::RemoveRequest(req.clone())).await;
        }
    }
}

/// Ends `p`'s share of `f` here: a folder's session closes (as for a removed device), a channel only stops being shared
/// over it.
pub(crate) async fn cut_off(f: &Space, p: &PeerHandle) {
    if f.kind == SpaceKind::Folder {
        let _ = p.control.send(Message::Close(Close { reason: "an admin of the folder asked to remove this device".into() })).await;
    } else {
        p.folders.write().unwrap_or_else(|e| e.into_inner()).remove(&f.id);
    }
}

/// Sends `p`, an owner of `f`, the requests still waiting (a session linked `f` with it, or the membership changed).
pub(crate) async fn send_waiting(f: &Space, p: &PeerHandle) {
    if f.role_of(&p.device_pk) != Some(Role::Owner) {
        return;
    }
    let waiting: Vec<RemoveRequest> = lock(&f.removals).iter().filter(|x| x.refused.is_none()).map(|x| x.request.clone()).collect();
    for req in waiting {
        let _ = p.control.send(Message::RemoveRequest(req)).await;
    }
}

/// The serial of the owners' latest statement about `device` this device holds (0: none).
fn seen(f: &Space, device: &[u8; 32]) -> u64 {
    f.membership.read().unwrap_or_else(|e| e.into_inner()).owner_serial(device).unwrap_or(0)
}

/// Why a request is no longer carried out: the owners decided about the device after the admin asked.
fn decided_since(f: &Space) -> String {
    format!("the owner of the {} has decided about this device since it was asked", crate::roles::noun(f))
}

/// The membership of `f` changed: requests for devices that are no longer members are done (the owners removed them),
/// and one the owners decided about since it was asked is no longer asked (kept with why: the device is not kept out
/// any more, and the admin may ask again).
pub(crate) fn settle(f: &Space) {
    let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
    let mut all = lock(&f.removals);
    let before = all.clone();
    all.retain(|x| m.role(&x.request.device).is_some());
    for x in all.iter_mut().filter(|x| x.refused.is_none() && m.owner_serial(&x.request.device).unwrap_or(0) > x.request.seen) {
        x.refused = Some(decided_since(f));
    }
    if *all != before {
        save(f, &all);
    }
}

/// Why an owner device does not carry out `req` from `peer` for `f` (`None`: it does).
fn refusal(ctx: &Ctx, f: &Space, peer: &PeerHandle, req: &RemoveRequest) -> Option<String> {
    let noun = crate::roles::noun(f);
    if req.by != peer.device_pk || !req.verify() {
        return Some("the request is not signed by the device that sent it".into());
    }
    if f.role_of(&req.by) != Some(Role::Admin) {
        return Some(format!("only an admin of the {noun} can ask its owner to remove a device"));
    }
    // The owners decided about the device after the admin asked (a grant, a removal and a new admission): that counts.
    if seen(f, &req.device) > req.seen {
        return Some(decided_since(f));
    }
    match f.role_of(&req.device) {
        None => None,
        Some(Role::Owner | Role::Admin) => Some(format!("an admin can ask to remove only members who are not owners or admins of the {noun}")),
        Some(_) if crate::devices::is_own_device(ctx, &req.device) => Some(format!("it is one of the {noun}'s owner's own devices")),
        Some(_) => None,
    }
}

/// On an owner device: `peer` asks for a removal. Carried out at once, by this device's own decision, when the sender
/// is an admin of the folder now and the device a user there; answered either way. A device without the folder key
/// (it cannot sign) leaves it to another owner device.
pub(crate) async fn on_request(ctx: &Arc<Ctx>, peer: &Arc<PeerHandle>, f: Arc<Space>, req: RemoveRequest) {
    if f.role_of(ctx.key.public_key()) != Some(Role::Owner) || f.secret.read().unwrap_or_else(|e| e.into_inner()).is_none() {
        return;
    }
    let refused = refusal(ctx, &f, peer, &req);
    if refused.is_none() {
        let (device, known) = (req.device, req.seen);
        // Decided again under the membership's lock: still a user, and no owner statement about it since the request.
        let make = |m: &trs_sync_proto::members::Membership, serials: &mut crate::roles::Serials| {
            let current = m.role(&device).is_some_and(|r| !r.admits()) && m.owner_serial(&device).unwrap_or(0) <= known;
            if current { vec![Body::Remove { device, serial: serials.next(), at_s: crate::roles::now_s() }] } else { Vec::new() }
        };
        if crate::roles::add_by_owner(&f, make) == Some(true) {
            crate::roles::changed(ctx, &f).await;
        }
    }
    let mut refused = refused;
    if let Some(why) = refused.as_mut() {
        why.truncate(why.floor_char_boundary(MAX_REMOVE_REASON));
    }
    let _ = peer.control.send(Message::RemoveReply(RemoveReply { folder_id: f.id, device: req.device, refused })).await;
}

/// On the admin's device: `peer` answered. Only an owner's device's answer counts (any other member could otherwise
/// call off the request and lift the device's cut-off). A refusal is kept with its reason (shown) and the device is no
/// longer cut off here; a removal done comes with the membership.
pub(crate) fn on_reply(f: &Space, peer: &PeerHandle, reply: RemoveReply) {
    if f.role_of(&peer.device_pk) != Some(Role::Owner) {
        return;
    }
    let Some(why) = reply.refused else { return };
    let mut all = lock(&f.removals);
    let mut changed = false;
    for x in all.iter_mut().filter(|x| x.request.device == reply.device && x.refused.is_none()) {
        x.refused = Some(why.clone());
        changed = true;
    }
    if changed {
        save(f, &all);
    }
}

/// **Tests:** sends every device sharing `f` an answer to a request for `device`'s removal as this device, whatever
/// its role (what a member that is not an owner could forge).
pub(crate) async fn test_reply(ctx: &Arc<Ctx>, f: &Arc<Space>, device: [u8; 32], refused: Option<String>) {
    for p in ctx.registry.peers() {
        if p.access(&f.id).is_some() {
            let _ = p.control.send(Message::RemoveReply(RemoveReply { folder_id: f.id, device, refused: refused.clone() })).await;
        }
    }
}

/// **Tests:** sends a request as this device, past the checks this device makes before it asks (what another build,
/// or a device that is not an admin, could send).
pub(crate) async fn test_send(ctx: &Arc<Ctx>, f: &Arc<Space>, device: [u8; 32]) {
    let req = RemoveRequest::new(&ctx.key, f.id, device, seen(f, &device), crate::roles::now_s());
    lock(&f.removals).push(Pending { request: req.clone(), refused: None });
    for p in ctx.registry.peers() {
        if p.access(&f.id).is_some() && f.role_of(&p.device_pk) == Some(Role::Owner) {
            let _ = p.control.send(Message::RemoveRequest(req.clone())).await;
        }
    }
}
