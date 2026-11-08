//! Your own devices trust each other (SPEC §7G.1 "Your devices", 0.1.13).
//!
//! The user's devices form a **devices group**: a third kind of [`Space`], next to folders and channels, with no
//! files and no messages. Its key pair is the **user key** U; every device in it is an owner and holds U (in the
//! secret store as `folder-<group id>`, never in `sync.toml`, never given to agents). Its membership
//! (`sync/<group id>.members`, [`Membership::new_devices`]) holds the owners' grants, the devices' profiles and the
//! [`Body::Follow`] statements; a removal in it is final.
//!
//! - **Pairing**: a device makes a one-use link valid for 15 minutes ([`PAIR_LINK_S`]); the other pastes it and
//!   dials. The device that made the link records the request ([`PairRequest`]) instead of admitting, and its user
//!   confirms (the two screens show the joining device's id) or refuses. On confirming it signs the new device in as
//!   an owner and sends it U ([`seedwell_sync_proto::msg::OwnerKey`]).
//! - **On all my devices**: a sibling signs a Follow for a folder or channel. Each sibling that does not have it joins
//!   it by itself: a folder at the default location for joined folders (under a name that is free, never merged into
//!   one that exists), on demand when it is on demand on the device that put it there ([`FOLLOW_ON_DEMAND`]; in full
//!   on a device that keeps no folders on demand, which its page says), else in full; a channel. It waits
//!   "following" ([`Space::following`]), proving to its siblings that it is one of them ([`Proof::Sibling`]).
//! - **Removed by its owner**: a folder's owner that removes a folder on all devices signs its Follow off with
//!   [`FOLLOW_REMOVED`], and every sibling removes it from Seedwell too, its files kept (an on-demand folder's become an
//!   ordinary folder at its place). Removed anywhere else, or by a sibling that is not its owner, it goes from that
//!   device only and does not come back there ("Not on This Mac").
//! - **Admission**: a sibling that is owner of the folder or channel signs `Grant{Admin}` for the others; one that is
//!   admin makes a one-use invitation of its own and signs `Grant{Read & write}` with it, exactly an admin's admission.
//!   It does so when the group or the Follow changes (push) and when a sibling proves itself (pull). An owner raises a
//!   sibling it finds at Read & write to Admin, never lowers one. These are ordinary statements, which every member
//!   (also on an older Seedwell) accepts. Matching user names never grant anything.
//! - **Read only** is the folder's own setting, its owners' (in its membership, [`Body::ReadOnly`]), not the group's:
//!   the group's old flag ([`seedwell_sync_proto::members::FOLLOW_READ_ONLY`], 0.1.19, which lowered roles) is read once, to give the roles it
//!   lowered back and carry it over ([`SyncHandle::migrate_read_only`]).
//! - **Removal**: final in the group; the dialog's places (where this device is owner) get the ordinary Remove too.
//!   Leave deletes the group here; Start Over makes a new one.
//!
//! Agents never see the group: no method of the agent interface pairs, confirms, removes, follows, leaves or reads
//! U; a device this Mac is connected to only for the group is in no list of theirs.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use seedwell_sync_proto::device::DeviceKey;
use seedwell_sync_proto::ids::{DeviceId, FolderId};
use seedwell_sync_proto::keys::{Access, FolderKey, FolderSecret, Proof};
use seedwell_sync_proto::members::{
    Body, FOLLOW_ON_DEMAND, FOLLOW_REMOVED, Follow, FollowTarget, Invite, InviteLink, Membership, Role, Signer, Statement, hex,
    new_invite,
};
use seedwell_sync_proto::msg::{DevicesJoin, JoinChannel, Members, Message};

use crate::config::{ChannelEntry, MyDevicesEntry};
use crate::roles::{Proved, now_s};
use crate::service::{SyncError, SyncHandle, invalid, public_key_from_hex, random_id};
use crate::shared::{Ctx, HubMsg, PeerHandle, Space, SpaceKind, lock};

/// How long a pairing link admits: 15 minutes.
pub const PAIR_LINK_S: u64 = 15 * 60;
/// How long the one-use invitation an admin makes for a sibling admits.
const SIBLING_INVITE_S: u64 = 3600;
/// The group's name on this device (it is never shown as a folder or channel).
const GROUP_NAME: &str = "Your devices";

/// A device that redeemed one of this device's pairing links, waiting for the user here (in memory only).
#[derive(Clone, Debug)]
pub struct PairRequest {
    /// The joining device's key.
    pub device_pk: [u8; 32],
    /// Its name, from its Hello.
    pub name: String,
    /// The link's invitation it redeemed.
    pub invite: Invite,
    /// When it asked first, Unix seconds.
    pub at_s: u64,
}

/// Where this device stands with its own devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevicesState {
    /// In no group (never paired, or it left).
    None,
    /// In a group of its own that no other device joined yet.
    Alone,
    /// Paired with at least one other device.
    Paired,
    /// Joining with a pairing link, not confirmed yet.
    Joining,
    /// Removed from its group by one of its devices: only Start Over helps.
    Removed,
}

/// One of the user's devices, for Settings › Devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnDevice {
    /// Its id.
    pub id: DeviceId,
    /// Its name (its own profile in the group; else as last met).
    pub name: String,
    /// Its user's name (empty: none given).
    pub user: String,
    /// This device.
    pub is_me: bool,
    /// Connected now (this device: always).
    pub online: bool,
    /// When a session with it last began or ended, Unix seconds.
    pub last_seen_s: Option<u64>,
    /// The device whose link paired it (`None`: the group's first device, or not known).
    pub paired_by: Option<DeviceId>,
    /// That device's name.
    pub paired_by_name: String,
    /// That device was removed from the group since: the user decides whether to keep this one.
    pub paired_by_removed: bool,
}

/// A pairing request, for the confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequestInfo {
    /// The device asking.
    pub id: DeviceId,
    /// Its name.
    pub name: String,
    /// When it asked, Unix seconds.
    pub at_s: u64,
    /// When the link it used stops admitting.
    pub expires_s: u64,
}

/// Everything Settings › Devices shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevicesStatus {
    /// Where this device stands.
    pub state: DevicesState,
    /// This device's id (shown while joining, so the user can compare it with the confirmation on the other).
    pub me: DeviceId,
    /// The devices in the group, this one first, then by name.
    pub devices: Vec<OwnDevice>,
    /// Devices waiting for the user here to confirm.
    pub requests: Vec<PairRequestInfo>,
    /// The pairing link this device made that still admits (not used, not expired), and when it expires.
    pub link: Option<(String, u64)>,
    /// Joining: what it does now, as a sentence.
    pub progress: Option<String>,
    /// Joining: the last thing that went wrong (a refusal).
    pub problem: Option<String>,
    /// Joining: the name of the device that made the link.
    pub maker: Option<String>,
}

/// A folder or channel a removal also removes the device from (or cannot).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// Its id.
    pub id: FolderId,
    /// Its name here.
    pub name: String,
    /// A channel (else a folder).
    pub channel: bool,
}

/// What removing a device does, for its dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovePlan {
    /// The device.
    pub id: DeviceId,
    /// Its name.
    pub name: String,
    /// Where this device is owner and the device is a member: it is removed there too (ticked).
    pub owner: Vec<Place>,
    /// Where the device is a member and this device only an admin: ask its owner.
    pub admin: Vec<Place>,
    /// Devices it paired, which the user keeps or removes too.
    pub paired: Vec<(DeviceId, String)>,
}

/// The "On all my devices" toggle of a folder or channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowState {
    /// It is on all the user's devices.
    pub on: bool,
    /// This device may turn it on (it is paired, and owner or admin there). Turning it off is always allowed.
    pub may: bool,
    /// This device is in a group with at least one other device.
    pub paired: bool,
    /// This device's role there.
    pub role: Option<Role>,
    /// The other devices' names, for the line under the toggle.
    pub others: Vec<String>,
}

// ------------------------------------------------------------------------------------------------- the group

/// The group's members other than `me`.
fn group_members(m: &Membership, me: &[u8; 32]) -> Vec<[u8; 32]> {
    m.members().filter(|(pk, _)| *pk != me).map(|(pk, _)| *pk).collect()
}

/// The key of a current member of the group, by its id.
fn member_pk(g: &Space, id: &DeviceId) -> Option<[u8; 32]> {
    g.membership.read().unwrap_or_else(|e| e.into_inner()).members().map(|(pk, _)| *pk).find(|pk| DeviceId::of_public_key(pk) == *id)
}

/// This device's group, if it is a member of it.
fn my_group(ctx: &Ctx) -> Option<Arc<Space>> {
    ctx.registry.devices().filter(|g| g.role_of(ctx.key.public_key()).is_some())
}

/// A folder's or channel's public key.
fn pk_of(f: &Space) -> [u8; 32] {
    *f.public.public_key()
}

/// The owners and admins of `f`, as this device knows them.
fn admitting(f: &Space) -> BTreeSet<[u8; 32]> {
    f.membership.read().unwrap_or_else(|e| e.into_inner()).members().filter(|(_, m)| m.role.admits()).map(|(pk, _)| *pk).collect()
}

/// The Follows for `f` in group `g` of the devices that are owner or admin of `f` here, oldest first. The folder's
/// devices are read first, then the group: never both locks at once.
fn trusted_follows(g: &Space, f: &Space) -> Vec<Follow> {
    let admits = admitting(f);
    g.membership.read().unwrap_or_else(|e| e.into_inner()).follows_by(&pk_of(f), |pk| admits.contains(pk))
}

/// The Follow that counts for folder or channel `f` (SPEC §7G.1): the latest of a device that is owner or admin of it,
/// so a sibling that is only a member there (one made Read only, say) can neither turn it off, nor remove it, nor
/// change what it says. Only while this device knows none of its owners and admins yet (it waits to be admitted) the
/// latest of any of the user's devices.
pub(crate) fn effective_follow(ctx: &Ctx, f: &Space) -> Option<Follow> {
    let g = my_group(ctx)?;
    let admits = admitting(f);
    let m = g.membership.read().unwrap_or_else(|e| e.into_inner());
    follow_in(&m, &pk_of(f), &admits)
}

/// [`effective_follow`] in group membership `m`, for target `pk` whose owners and admins are `admits`.
fn follow_in(m: &Membership, pk: &[u8; 32], admits: &BTreeSet<[u8; 32]>) -> Option<Follow> {
    if admits.is_empty() { m.follow(pk).cloned() } else { m.follow_by(pk, |p| admits.contains(p)) }
}

/// The serial and time this device's next Follow for target `pk` gets in group membership `m`: above every one of the
/// target's owners and admins (`admits`) and this device's own (`me`), so a higher serial another device said never
/// holds it back; at the highest serial ([`seedwell_sync_proto::members::MAX_FOLLOW_SERIAL`]) a second after the latest of
/// these, so it still ranks above it.
fn next_serial_in(m: &Membership, pk: &[u8; 32], admits: &BTreeSet<[u8; 32]>, me: &[u8; 32]) -> (u64, u64) {
    let top = m.follow_by(pk, |p| admits.contains(p) || p == me);
    let serial = top.as_ref().map_or(0, |t| t.serial).saturating_add(1).min(seedwell_sync_proto::members::MAX_FOLLOW_SERIAL);
    let at = match &top {
        Some(t) if t.serial >= serial => now_s().max(t.at_s.saturating_add(1)),
        _ => now_s(),
    };
    (serial, at)
}

/// What [`SyncHandle::removed_everywhere`] says of a folder its owner removes on all devices, taken before it goes here.
pub(crate) struct EverywhereFollow {
    pk: [u8; 32],
    name: String,
    /// The folder's owners and admins then.
    admits: BTreeSet<[u8; 32]>,
}

/// What [`add_follow`]'s `make` decides on: the group as it is when the Follow is added.
struct FollowNow<'a> {
    m: &'a Membership,
    /// The Follow that counts for the target now ([`effective_follow`]).
    counts: Option<Follow>,
    /// The serial and time this device's next Follow for it gets ([`next_serial_in`]).
    next: (u64, u64),
}

/// Says a Follow of this device's for target `pk` in group `g`, decided on the group as it is
/// ([`crate::roles::add_decided`]): `make` returns what to say, `None` for nothing. `admits` are the target's owners
/// and admins, read before (never both memberships locked at once). Two said at once (the user's switch and a change
/// of Files on demand, or two switches) never take the same serial, and neither is decided on a look from before the
/// other was added: the user turning it on just after an off was decided said nothing ("it is on already"), and the
/// off counted. True when the group changed (call `roles::changed`).
fn add_follow(ctx: &Ctx, g: &Space, pk: [u8; 32], admits: &BTreeSet<[u8; 32]>, make: impl FnOnce(FollowNow<'_>) -> Option<Body>) -> bool {
    let key: &DeviceKey = &ctx.key;
    crate::roles::add_decided(g, |m| {
        let now = FollowNow { m, counts: follow_in(m, &pk, admits), next: next_serial_in(m, &pk, admits, key.public_key()) };
        make(now).map(|b| Statement::by_device(key, &g.id, b)).into_iter().collect()
    })
}

/// `f` is on all the user's devices (the Follow that counts for it is on, [`effective_follow`]).
pub(crate) fn followed(ctx: &Ctx, f: &Space) -> bool {
    effective_follow(ctx, f).is_some_and(|fl| fl.on)
}

/// The name of the device that put `f` on all the user's devices (its profile in the group), if known.
pub(crate) fn followed_by(ctx: &Ctx, f: &Space) -> Option<String> {
    let g = my_group(ctx)?;
    let by = effective_follow(ctx, f)?.by;
    let m = g.membership.read().unwrap_or_else(|e| e.into_inner());
    m.profile(&by).map(|(name, _)| name.to_string())
}

/// Marks a space that opens here as following (one of the user's devices put it on all of them, and this device
/// is not admitted yet and joins with no invitation): its siblings are its devices to dial.
pub(crate) fn mark_following(ctx: &Ctx, f: &Space) -> bool {
    let me = ctx.key.public_key();
    let on = f.kind != SpaceKind::Devices && lock(&f.invite).is_none() && f.role_of(me).is_none() && followed(ctx, f);
    f.following.store(on, Ordering::SeqCst);
    if on && let Some(g) = my_group(ctx) {
        let others = group_members(&g.membership.read().unwrap_or_else(|e| e.into_inner()), me);
        let mut seen = f.members.write().unwrap_or_else(|e| e.into_inner());
        seen.extend(others.iter().map(DeviceId::of_public_key));
    }
    on
}

/// The proof a following folder or channel gives `peer_pk`: that this device is one of its own devices. Only to
/// a device that is a sibling now (a stranger learns nothing).
pub(crate) fn sibling_proof(ctx: &Ctx, f: &Space, peer_pk: &[u8; 32]) -> Option<Proof> {
    if !f.following.load(Ordering::SeqCst) {
        return None;
    }
    let g = my_group(ctx)?;
    g.role_of(peer_pk)?;
    Some(Proof::Sibling(g.id.0.to_vec()))
}

/// The start of a *Read only* run of 0.1.19 for folder `f` in group `g` (the devices-group flag that lowered roles,
/// [`FOLLOW_READ_ONLY`]): the earliest Follow with the flag of the folder's owners and admins since the last one
/// without, while the latest has it. Only the one-time migration reads it ([`SyncHandle::migrate_read_only`]).
fn legacy_run_start(g: &Space, f: &Space) -> Option<Follow> {
    let list = trusted_follows(g, f);
    let n = list.iter().rev().take_while(|fl| fl.read_only()).count();
    (n > 0).then(|| list[list.len() - n].clone())
}

/// How this device admits siblings to `f` now ([`admission`]): as an owner, `Grant{Admin}`; as an admin,
/// `Grant{Read & write}` with a one-use invitation of its own. (*Read only*, the owners' folder setting, changes no
/// role.)
struct Admission {
    owner: bool,
    role: Role,
}

/// [`Admission`] for `f`. `None`: this device admits nobody there.
fn admission(ctx: &Ctx, f: &Space) -> Option<Admission> {
    match f.role_of(ctx.key.public_key())? {
        Role::Owner => Some(Admission { owner: true, role: Role::Admin }),
        Role::Admin => Some(Admission { owner: false, role: Role::ReadWrite }),
        _ => None,
    }
}

impl Admission {
    /// The owner's grant for `device` ([`crate::roles::add_by_owner`] gives the serial).
    fn by_owner(&self, device: [u8; 32], serial: u64) -> Body {
        Body::Grant { device, role: self.role, serial, at_s: now_s(), invite: None }
    }

    /// The admin's grant for `device`, signed by this device.
    fn by_admin(&self, ctx: &Ctx, f: &Space, device: [u8; 32]) -> Statement {
        let (key, now): (&DeviceKey, u64) = (&ctx.key, now_s());
        let issuer = Signer::Device(*key.public_key());
        let invite = new_invite(pk_of(f), random_id(), seed(), self.role, 1, now + SIBLING_INVITE_S, now, issuer, &|m: &[u8]| key.sign(m));
        Statement::by_device(key, &f.id, Body::Grant { device, role: self.role, serial: 0, at_s: now, invite: Some(invite) })
    }
}

/// `pk` is one of this device's own devices (a member of its devices group, this device too).
pub(crate) fn is_own_device(ctx: &Ctx, pk: &[u8; 32]) -> bool {
    my_group(ctx).is_some_and(|g| g.role_of(pk).is_some())
}

/// Sibling `d` is one this device admits to `f` (membership `m`): not a member, not removed by an owner, the sharing
/// not ended.
fn admits_now(m: &Membership, d: &[u8; 32]) -> bool {
    m.role(d).is_none() && !m.is_removed(d) && !m.share_ended(now_s())
}

fn seed() -> [u8; 32] {
    let mut s = [0u8; 32];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut s);
    s
}

/// A sibling proved itself for `f` (pull): admitted here when `f` follows, this device is owner or admin of it,
/// and the device is a member of this device's group now and was not removed from `f` by an owner.
pub(crate) fn check_sibling(ctx: &Ctx, f: &Space, peer_pk: &[u8; 32], group: &[u8], changed: &mut bool) -> Proved {
    let Some(g) = my_group(ctx).filter(|g| g.id.0.as_slice() == group) else { return Proved::Nothing };
    if g.role_of(peer_pk).is_none() || !followed(ctx, f) {
        return Proved::Nothing;
    }
    let Some(adm) = admission(ctx, f) else { return Proved::Nothing };
    if adm.owner {
        let grant = |m: &Membership, serials: &mut crate::roles::Serials| if admits_now(m, peer_pk) { vec![adm.by_owner(*peer_pk, serials.next())] } else { Vec::new() };
        *changed |= crate::roles::add_by_owner(f, grant) == Some(true);
    } else if admits_now(&f.membership.read().unwrap_or_else(|e| e.into_inner()), peer_pk) {
        *changed |= crate::roles::add(f, [adm.by_admin(ctx, f, *peer_pk)]);
    }
    match f.role_of(peer_pk) {
        Some(r) => Proved::Member(r.access()),
        None => Proved::Nothing,
    }
}

/// A device redeemed a pairing link: only the device that made it records the request, for its user to confirm.
pub(crate) fn pair_request(ctx: &Ctx, peer_pk: &[u8; 32], invite: &Invite) -> Proved {
    if invite.issuer != Signer::Device(*ctx.key.public_key()) {
        return Proved::Nothing;
    }
    let now = now_s();
    let mut p = lock(&ctx.pairing);
    p.retain(|r| !r.invite.expired(now));
    if !p.iter().any(|r| r.device_pk == *peer_pk) {
        p.push(PairRequest { device_pk: *peer_pk, name: String::new(), invite: *invite, at_s: now });
    }
    Proved::Pending
}

/// Why a pairing link does not admit, for the device that pasted it.
pub(crate) fn refusal(e: seedwell_sync_proto::members::AdmitError) -> String {
    use seedwell_sync_proto::members::AdmitError;
    match e {
        AdmitError::Expired => "this link has expired: make a new one on the other device".into(),
        AdmitError::UsedUp => "this link has been used up: make a new one on the other device".into(),
        AdmitError::Revoked => "the pairing was refused on the other device".into(),
        AdmitError::Removed => concat!(
            "this ",
            seedwell_platform::computer!(),
            " left these devices or was removed from them: to pair it again, use Start Over on the other device"
        )
        .into(),
        e => e.to_string(),
    }
}

// ------------------------------------------------------------------------------------------------- sessions

/// `peer_pk` is one of this device's own now: a member of its current group or, while this device moves to a new
/// group, of the group it moves from.
fn own_peer(ctx: &Ctx, peer_pk: &[u8; 32]) -> bool {
    let Some(g) = ctx.registry.devices() else { return false };
    if g.role_of(peer_pk).is_some() {
        return true;
    }
    g.following.load(Ordering::SeqCst) && ctx.registry.ancestors().last().is_some_and(|a| a.role_of(peer_pk).is_some())
}

/// What this device says of a group it moved on from: its grant there, and where the group moved (each current
/// member's move), as proofs (statements).
fn ancestor_proofs(ctx: &Ctx, a: &Space) -> Vec<Proof> {
    let m = a.membership.read().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<Proof> = m.proofs_for(ctx.key.public_key()).iter().map(|st| Proof::Grant(st.to_bytes())).collect();
    let moves = m.statements().filter(|st| match (&st.body, st.signer) {
        (Body::Moved { .. }, Signer::Device(by)) => m.role(&by).is_some(),
        _ => false,
    });
    out.extend(moves.map(|st| Proof::Grant(st.to_bytes())));
    out.truncate(seedwell_sync_proto::msg::MAX_PROOFS);
    out
}

/// Our DevicesJoin for a session with `peer_pk`: the group's proofs, when `ids` names the group and the peer is one
/// of this device's own (or one waiting for its user to confirm it), or this device joins with a link or moves to
/// the group; and, to one of its own (`ancestors`: every one; else those `ids` names), what each group it moved on
/// from moved to. Empty otherwise: a stranger learns nothing.
pub(crate) fn own_devices_join(ctx: &Ctx, ids: &[FolderId], peer_pk: &[u8; 32], exporter: &[u8; 32], ancestors: bool) -> DevicesJoin {
    let mut groups = Vec::new();
    if let Some(g) = ctx.registry.devices().filter(|g| ids.contains(&g.id)) {
        let me = ctx.key.public_key();
        let joining = lock(&g.invite).is_some() && g.role_of(me).is_none();
        let waiting = lock(&ctx.pairing).iter().any(|r| r.device_pk == *peer_pk);
        if (joining || own_peer(ctx, peer_pk) || waiting)
            && let Some(proofs) = crate::roles::proofs(ctx, &g, exporter, peer_pk)
        {
            groups.push(JoinChannel { channel_id: g.id, proofs });
        }
    }
    if own_peer(ctx, peer_pk) {
        for a in ctx.registry.ancestors().into_iter().filter(|a| ancestors || ids.contains(&a.id)) {
            groups.push(JoinChannel { channel_id: a.id, proofs: ancestor_proofs(ctx, &a) });
        }
    }
    DevicesJoin { groups }
}

/// What a peer's DevicesJoin proved.
#[derive(Default)]
pub(crate) struct CheckedDevices {
    /// The group and the peer's access, when it is one of this device's own.
    pub(crate) shared: Option<(FolderId, Access)>,
    /// It asks to pair with one of this device's links: waiting for the user here.
    pub(crate) pending: Option<FolderId>,
    /// Refused, and why.
    pub(crate) refused: Vec<(FolderId, String)>,
    /// The group's membership changed.
    pub(crate) changed: bool,
    /// Groups this device moved on from that the peer (one of its own) named: told where they went in the answer, so
    /// the session stays.
    pub(crate) helping: Vec<FolderId>,
}

/// Checks a peer's DevicesJoin (`answered_only`: non-empty for a dialer, the groups it asked about). `name`: the
/// peer's name from its Hello, for a pairing request.
pub(crate) fn check_devices_join(ctx: &Ctx, j: &DevicesJoin, peer_pk: &[u8; 32], exporter: &[u8; 32], answered_only: &BTreeSet<FolderId>, name: &str) -> CheckedDevices {
    let mut out = CheckedDevices::default();
    let Some(g) = ctx.registry.devices() else { return out };
    // A group this device moved on from, from one of its own: what it says of it (where it moved) is kept.
    for jc in &j.groups {
        let Some(a) = ctx.registry.ancestors().into_iter().find(|a| a.id == jc.channel_id) else { continue };
        if !own_peer(ctx, peer_pk) {
            continue;
        }
        let sts: Vec<Statement> = jc.proofs.iter().filter_map(|p| match p {
            Proof::Grant(b) => Statement::from_bytes(b).ok(),
            _ => None,
        }).collect();
        if crate::roles::add(&a, sts) {
            ctx.devices_wake.notify_one();
        }
        out.helping.push(a.id);
    }
    for jc in j.groups.iter().filter(|jc| jc.channel_id == g.id) {
        if !answered_only.is_empty() && !answered_only.contains(&jc.channel_id) {
            continue;
        }
        match crate::roles::check(ctx, &g, peer_pk, exporter, &jc.proofs, &mut out.changed) {
            Proved::Member(a) => out.shared = Some((g.id, a)),
            Proved::Pending => {
                out.pending = Some(g.id);
                if let Some(r) = lock(&ctx.pairing).iter_mut().find(|r| r.device_pk == *peer_pk) {
                    r.name = name.to_string();
                }
            }
            Proved::Refused(why) => out.refused.push((g.id, why)),
            Proved::Nothing => {}
        }
    }
    out
}

/// Starts sharing the group over a session: the membership goes to the peer.
pub(crate) fn start_devices_link(ctx: &Arc<Ctx>, peer: &Arc<PeerHandle>, id: FolderId, access: Access) -> tokio::task::JoinHandle<()> {
    peer.folders.write().unwrap_or_else(|e| e.into_inner()).insert(id, access);
    let (ctx, peer) = (ctx.clone(), peer.clone());
    tokio::spawn(async move {
        let Some(g) = ctx.registry.devices().filter(|g| g.id == id) else { return };
        g.members.write().unwrap_or_else(|e| e.into_inner()).insert(peer.device);
        let _ = ctx.hub.send(HubMsg::MemberSeen { folder: id, device: peer.device });
        let statements: Vec<Statement> = g.membership.read().unwrap_or_else(|e| e.into_inner()).statements().cloned().collect();
        let _ = peer.control.send(Message::Members(Members { folder_id: id, statements })).await;
        // A sibling met: what follows is checked again (it may need admitting somewhere).
        ctx.devices_wake.notify_one();
    })
}

// ------------------------------------------------------------------------------------------------- the service

impl SyncHandle {
    fn group_entry(&self) -> Option<MyDevicesEntry> {
        lock(&self.inner.cfg).my_devices.clone()
    }

    /// Opens this device's group at start (or after making or joining one).
    pub(crate) async fn open_devices(&self, e: &MyDevicesEntry) -> Result<(), SyncError> {
        let id: FolderId = e.id.parse().map_err(|_| invalid("bad devices group id"))?;
        let pk = public_key_from_hex(&e.group_pk).filter(|pk| FolderId::of_public_key(pk) == id).ok_or_else(|| invalid("the devices group's key is missing"))?;
        let secret = match self.inner.secrets.get(&crate::roles::secret_name(&id))? {
            None => None,
            Some(b) => {
                let s = FolderSecret::from_bytes(b.try_into().map_err(|_| invalid("the user key is damaged"))?);
                (*s.public_key() == pk).then_some(s)
            }
        };
        let invite = match self.inner.secrets.get(&crate::roles::invite_name(&id))? {
            Some(b) => String::from_utf8(b).ok().and_then(|t| InviteLink::parse_devices(&t).ok()),
            None => None,
        };
        let links: BTreeMap<[u8; 16], String> = match self.inner.secrets.get(&crate::roles::links_name(&id))? {
            Some(b) => String::from_utf8_lossy(&b).lines().filter_map(|l| InviteLink::parse_devices(l).ok().map(|k| (k.invite.id, l.to_string()))).collect(),
            None => BTreeMap::new(),
        };
        let members_path = self.members_path(&id);
        let membership = crate::roles::load_devices(&members_path, pk)?;
        let space = Arc::new(Space::new(id, SpaceKind::Devices, FolderKey::Member(pk), secret, membership, members_path, invite, links, GROUP_NAME.into()));
        {
            let mut seen = space.members.write().unwrap_or_else(|e| e.into_inner());
            let mut book = lock(&self.inner.book);
            for d in &e.devices {
                let Ok(dev) = d.id.parse::<DeviceId>() else { continue };
                seen.insert(dev);
                for a in &d.addresses {
                    if let Ok(sa) = a.parse() {
                        book.by_device.entry(dev).or_default().insert(sa);
                    }
                }
            }
        }
        // The groups it moved on from (without their keys), and whether it is moving into this one: not a member yet,
        // no pairing link, so its members admit it from the group it moved from. One already open here (the group it
        // switches from, or an earlier one) is kept as it is, never read from its file again: a session may add to it
        // while this device switches (another device's Moved, said at the same moment), and that is what this device
        // must read of it afterwards.
        let open: Vec<Arc<Space>> = self.inner.ctx.registry.devices().into_iter().chain(self.inner.ctx.registry.ancestors()).collect();
        let mut ancestors = Vec::new();
        for p in &e.previous {
            let (Ok(aid), Some(apk)) = (p.id.parse::<FolderId>(), public_key_from_hex(&p.group_pk)) else { continue };
            if let Some(a) = open.iter().find(|a| a.id == aid && pk_of(a) == apk) {
                *a.secret.write().unwrap_or_else(|e| e.into_inner()) = None;
                lock(&a.invite).take();
                lock(&a.links).clear();
                a.following.store(false, Ordering::SeqCst);
                ancestors.push(a.clone());
                continue;
            }
            let path = self.members_path(&aid);
            let Ok(m) = crate::roles::load_devices(&path, apk) else { continue };
            ancestors.push(Arc::new(Space::new(aid, SpaceKind::Devices, FolderKey::Member(apk), None, m, path, None, BTreeMap::new(), GROUP_NAME.into())));
        }
        // (Tests: a switch waits here, its groups ready and not in use yet.)
        if self.inner.ctx.hold_switch.load(Ordering::SeqCst) {
            self.inner.ctx.switches_waiting.fetch_add(1, Ordering::SeqCst);
            while self.inner.ctx.hold_switch.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            self.inner.ctx.switches_waiting.fetch_sub(1, Ordering::SeqCst);
        }
        let moving = !ancestors.is_empty() && lock(&space.invite).is_none() && space.role_of(self.inner.ctx.key.public_key()).is_none();
        space.following.store(moving, Ordering::SeqCst);
        self.inner.ctx.registry.set_ancestors(ancestors);
        self.learn_from_link(&space);
        self.inner.ctx.registry.set_devices(Some(space.clone()));
        if crate::roles::publish_profile(&self.inner.ctx, &space) {
            self.spawn_changed(&space);
        }
        self.inner.wake_dialer.notify_one();
        self.inner.lan.wake.notify_one();
        self.offer_devices().await;
        self.inner.ctx.devices_wake.notify_one();
        Ok(())
    }

    /// Offers the group to connected devices: the siblings among them, or (joining) the one that made the link.
    async fn offer_devices(&self) {
        let ctx = &self.inner.ctx;
        let Some(g) = ctx.registry.devices() else { return };
        for p in ctx.registry.peers() {
            if !p.has(seedwell_sync_proto::msg::FEATURE_DEVICES) || p.access(&g.id).is_some() {
                continue;
            }
            let join = own_devices_join(ctx, &[g.id], &p.device_pk, &p.exporter, true);
            if !join.groups.is_empty() {
                let _ = p.control.send(Message::DevicesJoin(join)).await;
            }
        }
    }

    /// Makes this device's group (it alone, an owner holding the new user key) and opens it.
    async fn new_group(&self) -> Result<Arc<Space>, SyncError> {
        let secret = FolderSecret::generate(&mut ring::rand::SystemRandom::new()).map_err(io::Error::other)?;
        let id = FolderId::of_public_key(secret.public_key());
        let me = *self.inner.ctx.key.public_key();
        let mut m = Membership::new_devices(*secret.public_key());
        m.add([Statement::by_owner(&secret, Body::Grant { device: me, role: Role::Owner, serial: 1, at_s: now_s(), invite: None })]);
        crate::roles::save(&self.members_path(&id), &m)?;
        self.inner.secrets.put(&crate::roles::secret_name(&id), secret.as_bytes())?;
        let entry = MyDevicesEntry { id: id.to_string(), group_pk: hex(secret.public_key()), mode: "full".into(), ..MyDevicesEntry::default() };
        lock(&self.inner.cfg).my_devices = Some(entry.clone());
        self.save_config()?;
        self.open_devices(&entry).await?;
        self.inner.ctx.registry.devices().ok_or(SyncError::NotFound)
    }

    /// Where this device stands with its own devices, for Settings › Devices.
    pub fn devices_status(&self) -> DevicesStatus {
        let ctx = &self.inner.ctx;
        let me = *ctx.key.public_key();
        let mut out = DevicesStatus { state: DevicesState::None, me: ctx.me, devices: Vec::new(), requests: Vec::new(), link: None, progress: None, problem: None, maker: None };
        let Some(g) = ctx.registry.devices() else { return out };
        let now = now_s();
        let (role, members, removed_for_good, grants) = {
            let m = g.membership.read().unwrap_or_else(|e| e.into_inner());
            let members: Vec<[u8; 32]> = m.members().map(|(pk, _)| *pk).collect();
            // Who paired whom: the issuer of the invitation each device's grant carries.
            let grants: BTreeMap<[u8; 32], Option<[u8; 32]>> = m
                .statements()
                .filter_map(|st| match (&st.body, st.signer) {
                    (Body::Grant { device, invite: Some(i), .. }, Signer::Folder) => match i.issuer {
                        Signer::Device(by) => Some((*device, Some(by))),
                        Signer::Folder => None,
                    },
                    _ => None,
                })
                .collect();
            let gone: BTreeSet<[u8; 32]> = grants.values().flatten().filter(|by| m.removed_for_good(by)).copied().collect();
            (m.role(&me), members, gone, grants)
        };
        let moving = role.is_none() && lock(&g.invite).is_none() && g.following.load(Ordering::SeqCst);
        let joining = (lock(&g.invite).is_some() && role.is_none()) || moving;
        out.state = match (role, joining) {
            (Some(_), _) if members.len() > 1 => DevicesState::Paired,
            (Some(_), _) => DevicesState::Alone,
            (None, true) => DevicesState::Joining,
            (None, false) => DevicesState::Removed,
        };
        let names = self.own_names(&g);
        let online: BTreeSet<DeviceId> = ctx.registry.peers().iter().filter(|p| p.access(&g.id).is_some()).map(|p| p.device).collect();
        let seen: BTreeMap<String, u64> = self.group_entry().map(|e| e.devices.into_iter().map(|d| (d.id, d.last_seen)).collect()).unwrap_or_default();
        if role.is_some() {
            for pk in &members {
                let id = DeviceId::of_public_key(pk);
                let is_me = *pk == me;
                let (name, user) = names.get(&id).cloned().unwrap_or_default();
                let by = grants.get(pk).copied().flatten();
                out.devices.push(OwnDevice {
                    id,
                    name,
                    user,
                    is_me,
                    online: is_me || online.contains(&id),
                    last_seen_s: seen.get(&id.to_string()).copied().filter(|s| *s > 0),
                    paired_by: by.map(|b| DeviceId::of_public_key(&b)),
                    paired_by_name: by.and_then(|b| names.get(&DeviceId::of_public_key(&b)).map(|n| n.0.clone())).unwrap_or_default(),
                    paired_by_removed: by.is_some_and(|b| removed_for_good.contains(&b)),
                });
            }
            out.devices.sort_by(|a, b| b.is_me.cmp(&a.is_me).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then_with(|| a.id.cmp(&b.id)));
            let mut p = lock(&ctx.pairing);
            p.retain(|r| !r.invite.expired(now) && g.role_of(&r.device_pk).is_none());
            out.requests = p
                .iter()
                .map(|r| PairRequestInfo { id: DeviceId::of_public_key(&r.device_pk), name: r.name.clone(), at_s: r.at_s, expires_s: r.invite.expires_s })
                .collect();
            drop(p);
            // The newest link of ours that still admits.
            let m = g.membership.read().unwrap_or_else(|e| e.into_inner());
            out.link = lock(&g.links)
                .iter()
                .filter_map(|(id, text)| {
                    let l = InviteLink::parse_devices(text).ok()?;
                    let used = m.invite(id).is_some_and(|s| s.used_up() || s.revoked);
                    (!l.invite.expired(now) && !used).then_some((l.invite.at_s, text.clone(), l.invite.expires_s))
                })
                .max()
                .map(|(_, t, e)| (t, e));
        }
        if moving {
            out.progress = Some("Moving to your devices' new key (one of them was removed): waiting for one of your devices…".into());
        } else if joining {
            let maker = lock(&g.invite).as_ref().and_then(|l| l.reach.as_ref().map(|r| r.name.clone()).or_else(|| l.name.clone()));
            out.maker = maker.clone();
            let (progress, problem, _) = self.join_status(&g);
            out.problem = problem;
            // Met the device that made the link: its user confirms there.
            let met = ctx.registry.peers().into_iter().find(|p| p.access(&g.id).is_some()).map(|p| p.name.clone());
            out.progress = match met {
                Some(name) => Some(format!("Confirm on “{name}”: it shows this {}'s id", seedwell_platform::computer!())),
                None => progress,
            };
        }
        out
    }

    /// Each device's names in the group: (device name, user name), its own profile first, else as met.
    fn own_names(&self, g: &Space) -> BTreeMap<DeviceId, (String, String)> {
        let mut out: BTreeMap<DeviceId, (String, String)> = BTreeMap::new();
        if let Some(e) = self.group_entry() {
            for d in e.devices {
                if let Ok(id) = d.id.parse::<DeviceId>() {
                    out.insert(id, (d.name, String::new()));
                }
            }
        }
        for p in self.inner.ctx.registry.peers() {
            out.insert(p.device, (p.name.clone(), lock(&p.user_name).clone()));
        }
        for (id, n) in g.profile_names() {
            out.insert(id, (n.device, n.user));
        }
        out.insert(self.inner.ctx.me, (lock(&self.inner.ctx.device_name).clone(), lock(&self.inner.ctx.user_name).clone()));
        out
    }

    /// *Pair a Device…*: a one-use link, valid for 15 minutes, that another device pastes to join this device's
    /// group (made now if there is none). The link is a secret: only the user's own screen shows it. Returns the link
    /// and when it expires.
    pub async fn devices_pair_link(&self) -> Result<(String, u64), SyncError> {
        let ctx = self.inner.ctx.clone();
        let me = *ctx.key.public_key();
        let g = match ctx.registry.devices() {
            Some(g) if g.role_of(&me).is_some() => g,
            Some(g) if lock(&g.invite).is_some() => return Err(invalid(concat!("this ", seedwell_platform::computer!(), " is joining your devices with a link: wait for that, or leave first"))),
            Some(_) => return Err(invalid(concat!("this ", seedwell_platform::computer!(), " was removed from your devices: start over first"))),
            None => self.new_group().await?,
        };
        let now = now_s();
        let key = ctx.key.clone();
        // Made by this device (its key signs it, so the group records who paired whom), at Read & write (the role a
        // device may sign an invitation for; the confirmation makes the new device an owner).
        let token_seed = seed();
        let invite = new_invite(pk_of(&g), random_id(), token_seed, Role::ReadWrite, 1, now + PAIR_LINK_S, now, Signer::Device(me), &|m: &[u8]| key.sign(m));
        let name = lock(&ctx.device_name).clone();
        let link = InviteLink { invite, token_seed, name: Some(name).filter(|n| !n.is_empty()), agents: false, reach: Some(self.own_reach()) };
        let text = link.format_devices();
        {
            // (Kept with the links unlocked: Settings reads them, and the secret store may be slow.)
            let _turn = lock(&g.saving);
            let all = {
                let mut links = lock(&g.links);
                // Expired ones go: a pairing link is good for minutes only.
                links.retain(|_, t| InviteLink::parse_devices(t).is_ok_and(|l| !l.invite.expired(now)));
                links.insert(invite.id, text.clone());
                links.values().map(String::as_str).collect::<Vec<_>>().join("\n")
            };
            self.inner.secrets.put(&crate::roles::links_name(&g.id), all.as_bytes())?;
        }
        Ok((text, invite.expires_s))
    }

    /// *Pair with a Link…*: joins the group of the device that made `link`. Its user there confirms. A group of this
    /// device's own that no other device joined goes; one with other devices in it is left first (Settings).
    pub async fn devices_join(&self, link: &str) -> Result<DevicesStatus, SyncError> {
        let l = InviteLink::parse_devices(link).map_err(|e| invalid(e.to_string()))?;
        if l.invite.expired(now_s()) {
            return Err(invalid("this link has expired: make a new one on the other device"));
        }
        let ctx = self.inner.ctx.clone();
        let me = *ctx.key.public_key();
        if l.invite.issuer == Signer::Device(me) {
            return Err(invalid(concat!("this link was made on this ", seedwell_platform::computer!(), ": paste it on the other device")));
        }
        let id = l.invite.folder_id();
        if let Some(g) = ctx.registry.devices() {
            let others = group_members(&g.membership.read().unwrap_or_else(|e| e.into_inner()), &me);
            if g.id == id && g.role_of(&me).is_some() {
                return Err(invalid(concat!("this ", seedwell_platform::computer!(), " is already one of these devices")));
            }
            if g.role_of(&me).is_some() && !others.is_empty() {
                return Err(invalid(concat!("this ", seedwell_platform::computer!(), " is paired with other devices: leave them first (Settings › Devices)")));
            }
            self.drop_group(&g)?;
        }
        self.inner.secrets.put(&crate::roles::invite_name(&id), l.format_devices().as_bytes())?;
        let _ = std::fs::remove_file(self.members_path(&id));
        let entry = MyDevicesEntry { id: id.to_string(), group_pk: hex(&l.invite.folder_pk), mode: "full".into(), ..MyDevicesEntry::default() };
        lock(&self.inner.cfg).my_devices = Some(entry.clone());
        self.save_config()?;
        self.open_devices(&entry).await?;
        Ok(self.devices_status())
    }

    /// *Pair*: admits the device waiting with one of this device's links, as an owner of the group (it receives the
    /// user key).
    pub async fn devices_confirm(&self, device: &DeviceId) -> Result<(), SyncError> {
        let ctx = self.inner.ctx.clone();
        let g = my_group(&ctx).ok_or_else(|| invalid("this device is not in a group of your devices"))?;
        let req = take_request(&ctx, device).ok_or_else(|| invalid("that device is no longer waiting: ask it to paste the link again"))?;
        let now = now_s();
        let verdict = g.membership.read().unwrap_or_else(|e| e.into_inner()).check_admit(ctx.key.public_key(), &req.invite, &req.device_pk, now);
        match verdict {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(e) => return Err(invalid(e.to_string())),
        }
        let grant = |m: &Membership, serials: &mut crate::roles::Serials| match m.check_admit(ctx.key.public_key(), &req.invite, &req.device_pk, now) {
            Ok(None) => vec![Body::Grant { device: req.device_pk, role: Role::Owner, serial: serials.next(), at_s: now, invite: Some(req.invite) }],
            _ => Vec::new(),
        };
        crate::roles::add_by_owner(&g, grant).ok_or_else(|| invalid("the user key is missing on this device"))?;
        // Sessions turned down before (it was not admitted yet) are tried again at once.
        self.forget_refusals();
        // Connected (it waits on a session): the group is shared over it from now on, and it gets the key.
        if let Some(p) = ctx.registry.peer(device) {
            p.folders.write().unwrap_or_else(|e| e.into_inner()).insert(g.id, Access::Write);
            g.members.write().unwrap_or_else(|e| e.into_inner()).insert(*device);
            let _ = ctx.hub.send(HubMsg::MemberSeen { folder: g.id, device: *device });
        }
        crate::roles::changed(&ctx, &g).await;
        Ok(())
    }

    /// *Refuse*: the waiting device is not admitted, and the link it used admits nobody any more.
    pub async fn devices_refuse(&self, device: &DeviceId) -> Result<(), SyncError> {
        let ctx = self.inner.ctx.clone();
        let g = my_group(&ctx).ok_or_else(|| invalid("this device is not in a group of your devices"))?;
        let Some(req) = take_request(&ctx, device) else { return Ok(()) };
        let st = Statement::by_device(&ctx.key, &g.id, Body::Revoke { id: req.invite.id, at_s: now_s() });
        if crate::roles::add(&g, [st]) {
            crate::roles::changed(&ctx, &g).await;
        }
        if let Some(p) = ctx.registry.peer(device)
            && p.folders.read().unwrap_or_else(|e| e.into_inner()).is_empty()
        {
            let reason = format!("link: “{}” refused to pair", lock(&ctx.device_name));
            let _ = p.control.send(Message::Close(seedwell_sync_proto::msg::Close { reason })).await;
        }
        Ok(())
    }

    /// What removing `device` from the group does, for its dialog.
    pub fn devices_remove_plan(&self, device: &DeviceId) -> Result<RemovePlan, SyncError> {
        let ctx = &self.inner.ctx;
        let g = my_group(ctx).ok_or_else(|| invalid("this device is not in a group of your devices"))?;
        let pk = member_pk(&g, device).ok_or(SyncError::NotFound)?;
        if g.role_of(&pk).is_none() {
            return Err(SyncError::NotFound);
        }
        let me = ctx.key.public_key();
        let names = self.own_names(&g);
        let mut plan = RemovePlan { id: *device, name: names.get(device).map(|n| n.0.clone()).unwrap_or_default(), owner: Vec::new(), admin: Vec::new(), paired: Vec::new() };
        for f in ctx.registry.spaces().into_iter().filter(|f| f.kind != SpaceKind::Devices && f.role_of(&pk).is_some()) {
            let place = Place { id: f.id, name: lock(&f.name).clone(), channel: f.kind == SpaceKind::Channel };
            match f.role_of(me) {
                Some(Role::Owner) => plan.owner.push(place),
                Some(Role::Admin) => plan.admin.push(place),
                _ => {}
            }
        }
        plan.owner.sort_by_key(|p| p.name.to_lowercase());
        plan.admin.sort_by_key(|p| p.name.to_lowercase());
        plan.paired = self.devices_status().devices.into_iter().filter(|d| d.paired_by == Some(*device)).map(|d| (d.id, d.name)).collect();
        Ok(plan)
    }

    /// *Remove…*: `device` is out of the group for good (it never joins it again, and no device of the group admits
    /// it anywhere again), and out of `places` (folders and channels where this device is an owner). What it already
    /// has stays with it.
    pub async fn devices_remove(&self, device: &DeviceId, places: &[FolderId]) -> Result<(), SyncError> {
        let ctx = self.inner.ctx.clone();
        let _one = ctx.devices_lock.lock().await;
        let g = my_group(&ctx).ok_or_else(|| invalid("this device is not in a group of your devices"))?;
        let pk = member_pk(&g, device).ok_or(SyncError::NotFound)?;
        if pk == *ctx.key.public_key() {
            return Err(invalid("to take this device out, leave your devices instead"));
        }
        if g.role_of(&pk).is_none() {
            return Err(SyncError::NotFound);
        }
        crate::roles::add_by_owner(&g, |_, serials| vec![Body::Remove { device: pk, serial: serials.next(), at_s: now_s() }])
            .ok_or_else(|| invalid("the user key is missing on this device"))?;
        lock(&ctx.pairing).retain(|r| r.device_pk != pk);
        // The removed device still holds the user key: the group goes on under a new one, which it never gets. First, so
        // the devices connected now (the removed one too) learn it before a folder's removal ends its session.
        self.rotate(&g).await?;
        for id in places {
            let Some(f) = ctx.registry.user_space(id) else { continue };
            if f.role_of(ctx.key.public_key()) != Some(Role::Owner) || f.role_of(&pk).is_none() {
                continue;
            }
            if crate::roles::add_by_owner(&f, |_, serials| vec![Body::Remove { device: pk, serial: serials.next(), at_s: now_s() }]) == Some(true) {
                crate::roles::changed(&ctx, &f).await;
            }
        }
        Ok(())
    }

    /// Changes the group's key: a new group with the current members (the removed ones left out), the folders and
    /// channels on all devices, and this device's knowledge of the others; a Moved statement in the old group tells
    /// the members where it went (now to those connected, later to each through any member of the new group); the
    /// old key is deleted here. Each member moves by itself and is admitted by the new group's members, who know its
    /// key from the old group.
    async fn rotate(&self, g: &Arc<Space>) -> Result<(), SyncError> {
        let secret = FolderSecret::generate(&mut ring::rand::SystemRandom::new()).map_err(io::Error::other)?;
        self.rotate_to(g, secret).await
    }

    /// [`Self::rotate`], to the group of `secret`.
    async fn rotate_to(&self, g: &Arc<Space>, secret: FolderSecret) -> Result<(), SyncError> {
        let ctx = self.inner.ctx.clone();
        let me = *ctx.key.public_key();
        let (pk, id) = (*secret.public_key(), FolderId::of_public_key(secret.public_key()));
        let now = now_s();
        let (mut members, follows, removed) = {
            let m = g.membership.read().unwrap_or_else(|e| e.into_inner());
            let members: Vec<[u8; 32]> = m.members().map(|(pk, _)| *pk).filter(|pk| *pk != me).collect();
            // What follows, and the folders removed on all devices (a sibling that did not take one out yet still does).
            (members, m.follows().cloned().collect::<Vec<_>>(), m.removed_devices())
        };
        members.insert(0, me);
        let mut m = Membership::new_devices(pk);
        let grants = members.iter().enumerate().map(|(i, d)| Statement::by_owner(&secret, Body::Grant { device: *d, role: Role::Owner, serial: 1 + i as u64, at_s: now, invite: None }));
        m.add(grants.collect::<Vec<_>>());
        // Each as it counts here (an owner's or admin's statement for a folder or channel here, never a member's alone,
        // which this device would otherwise sign as its own), at its own time. (*Read only* is the folder's own setting,
        // in its membership: a new group key does not touch it.)
        let carried = follows.iter().filter_map(|plain| {
            let here = ctx.registry.user_space(&FolderId::of_public_key(&plain.target_pk));
            let f = match &here {
                Some(s) => effective_follow(&ctx, s)?,
                None => plain.clone(),
            };
            if !(f.on || f.removed()) {
                return None;
            }
            let body = Body::Follow { target: f.target, target_pk: f.target_pk, name: f.name.clone(), on: f.on, serial: 1, at_s: f.at_s, flags: f.flags };
            Some(Statement::by_device(&ctx.key, &id, body))
        });
        m.add(carried.collect::<Vec<_>>());
        crate::roles::save(&self.members_path(&id), &m)?;
        self.inner.secrets.put(&crate::roles::secret_name(&id), secret.as_bytes())?;
        // Said in the old group, so its members (the connected ones now) move too.
        if crate::roles::add(g, [Statement::by_device(&ctx.key, &g.id, Body::Moved { target_pk: pk, at_s: now })]) {
            crate::roles::changed(&ctx, g).await;
        }
        self.switch_group(g, pk, &removed).await
    }

    /// Moves this device from its group `g` to the group with key `target` (which a member's Moved names): it says so
    /// in `g` too (so the others there follow), and is admitted there by the members, who know its key.
    async fn move_to(&self, g: &Arc<Space>, target: [u8; 32]) -> Result<(), SyncError> {
        let ctx = self.inner.ctx.clone();
        let me = *ctx.key.public_key();
        let mine = g.membership.read().unwrap_or_else(|e| e.into_inner()).moves().iter().any(|(by, t)| *by == me && *t == target);
        if g.role_of(&me).is_some() && !mine && crate::roles::add(g, [Statement::by_device(&ctx.key, &g.id, Body::Moved { target_pk: target, at_s: now_s() })]) {
            crate::roles::changed(&ctx, g).await;
        }
        let removed = g.membership.read().unwrap_or_else(|e| e.into_inner()).removed_devices();
        self.switch_group(g, target, &removed).await
    }

    /// Makes the group with key `target` this device's group, `g` one it moved on from (kept without its key, at most
    /// [`crate::config::MAX_PREVIOUS_GROUPS`]), and forgets what it knew of the `removed` devices there.
    async fn switch_group(&self, g: &Arc<Space>, target: [u8; 32], removed: &[[u8; 32]]) -> Result<(), SyncError> {
        let ctx = &self.inner.ctx;
        let id = FolderId::of_public_key(&target);
        if id == g.id {
            return Ok(());
        }
        let gone: BTreeSet<String> = removed.iter().map(|pk| DeviceId::of_public_key(pk).to_string()).collect();
        let (entry, forgotten) = {
            let mut cfg = lock(&self.inner.cfg);
            let Some(e) = cfg.my_devices.as_mut() else { return Err(SyncError::NotFound) };
            e.previous.retain(|p| p.id != g.id.to_string() && p.id != id.to_string());
            e.previous.push(crate::config::PreviousGroup { id: g.id.to_string(), group_pk: hex(&pk_of(g)) });
            let mut forgotten = Vec::new();
            while e.previous.len() > crate::config::MAX_PREVIOUS_GROUPS {
                forgotten.extend(e.previous.remove(0).id.parse::<FolderId>());
            }
            e.id = id.to_string();
            e.group_pk = hex(&target);
            e.devices.retain(|d| !gone.contains(&d.id));
            (e.clone(), forgotten)
        };
        // (With the configuration unlocked: the app's calls read it.)
        for oid in forgotten {
            let _ = std::fs::remove_file(self.members_path(&oid));
        }
        self.save_config()?;
        // The old key is worth nothing from now on: it goes.
        for name in [crate::roles::secret_name(&g.id), crate::roles::invite_name(&g.id), crate::roles::links_name(&g.id)] {
            self.inner.secrets.delete(&name)?;
        }
        *g.secret.write().unwrap_or_else(|e| e.into_inner()) = None;
        lock(&ctx.pairing).clear();
        self.open_devices(&entry).await
    }

    /// *Leave Your Devices…*: this device says it leaves (to the devices connected now), and the group is deleted
    /// here: its membership, its table in `sync.toml` and its keys. Folders and channels stay (those that were only
    /// waiting to be admitted from the group go: nothing of them is here).
    pub async fn devices_leave(&self) -> Result<(), SyncError> {
        let ctx = self.inner.ctx.clone();
        let _one = ctx.devices_lock.lock().await;
        let Some(g) = ctx.registry.devices() else { return Ok(()) };
        let me = *ctx.key.public_key();
        if g.role_of(&me).is_some()
            && crate::roles::add_by_owner(&g, |_, serials| vec![Body::Remove { device: me, serial: serials.next(), at_s: now_s() }]) == Some(true)
        {
            let statements: Vec<Statement> = g.membership.read().unwrap_or_else(|e| e.into_inner()).statements().cloned().collect();
            for p in ctx.registry.peers().into_iter().filter(|p| p.access(&g.id).is_some()) {
                let _ = p.control.send(Message::Members(Members { folder_id: g.id, statements: statements.clone() })).await;
            }
        }
        let waiting: Vec<(FolderId, SpaceKind)> = ctx.registry.spaces().iter().filter(|f| f.following.load(Ordering::SeqCst) && f.role_of(&me).is_none()).map(|f| (f.id, f.kind)).collect();
        self.drop_group(&g)?;
        for (id, kind) in waiting {
            match kind {
                SpaceKind::Folder => {
                    let _ = self.remove_folder(&id).await;
                }
                SpaceKind::Channel => {
                    let _ = self.chat_leave(&id);
                }
                SpaceKind::Devices => {}
            }
        }
        Ok(())
    }

    /// *Start Over…*: leaves the group (see [`Self::devices_leave`]) and makes a new one with this device alone, to
    /// pair the others with again: the way out when the user key must count as stolen, or this device was removed.
    pub async fn devices_start_over(&self) -> Result<DevicesStatus, SyncError> {
        self.devices_leave().await?;
        self.new_group().await?;
        Ok(self.devices_status())
    }

    /// Deletes the group here: its sessions, its table, its membership file and its keys.
    fn drop_group(&self, g: &Space) -> Result<(), SyncError> {
        let ctx = &self.inner.ctx;
        ctx.registry.set_devices(None);
        lock(&ctx.pairing).clear();
        for p in ctx.registry.peers() {
            p.folders.write().unwrap_or_else(|e| e.into_inner()).remove(&g.id);
        }
        for f in ctx.registry.spaces() {
            f.following.store(false, Ordering::SeqCst);
        }
        lock(&self.inner.cfg).my_devices = None;
        self.save_config()?;
        for name in [crate::roles::secret_name(&g.id), crate::roles::invite_name(&g.id), crate::roles::links_name(&g.id)] {
            self.inner.secrets.delete(&name)?;
        }
        let _ = std::fs::remove_file(&g.members_path);
        for a in ctx.registry.ancestors() {
            let _ = std::fs::remove_file(&a.members_path);
        }
        ctx.registry.set_ancestors(Vec::new());
        self.forget_strangers();
        Ok(())
    }

    /// The "On all my devices" toggle of folder or channel `id`.
    pub fn follow_state(&self, id: &FolderId) -> Result<FollowState, SyncError> {
        let ctx = &self.inner.ctx;
        let f = ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        let me = ctx.key.public_key();
        let role = f.role_of(me);
        let Some(g) = my_group(ctx) else { return Ok(FollowState { on: false, may: false, paired: false, role, others: Vec::new() }) };
        let names = self.own_names(&g);
        let others: Vec<String> = {
            let m = g.membership.read().unwrap_or_else(|e| e.into_inner());
            group_members(&m, me).iter().map(|pk| names.get(&DeviceId::of_public_key(pk)).map(|n| n.0.clone()).unwrap_or_default()).filter(|n| !n.is_empty()).collect()
        };
        let paired = !others.is_empty();
        let on = followed(ctx, &f);
        Ok(FollowState { on, may: paired && role.is_some_and(Role::admits), paired, role, others })
    }

    /// Turns "On all my devices" on or off for folder or channel `id`: on puts it on every device of the group (only
    /// an owner or admin here, so siblings can be admitted), on demand where it can be when it is on demand here
    /// ([`FOLLOW_ON_DEMAND`]: only the default when a sibling joins it); off only stops it coming to new ones, and
    /// deletes nothing anywhere. (*Read only* is the folder's own setting: [`Self::set_read_only`].)
    pub async fn set_follow(&self, id: &FolderId, on: bool) -> Result<(), SyncError> {
        self.say_follow(id, on).await
    }

    /// Says "On all my devices" for `id` in the group as `on`, when that is not what the group says already.
    async fn say_follow(&self, id: &FolderId, on: bool) -> Result<(), SyncError> {
        let ctx = self.inner.ctx.clone();
        let g = my_group(&ctx).ok_or_else(|| invalid(concat!("pair this ", seedwell_platform::computer!(), " with your other devices first (Settings › Devices)")))?;
        let f = ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        if on && !f.role_of(ctx.key.public_key()).is_some_and(Role::admits) {
            return Err(invalid(format!("only an owner or admin of this {} can put it on all your devices", crate::roles::noun(&f))));
        }
        let target = if f.kind == SpaceKind::Channel { FollowTarget::Channel } else { FollowTarget::Folder };
        let pk = pk_of(&f);
        let name = lock(&f.name).clone();
        let flags = if on && f.kind == SpaceKind::Folder && self.offers_on_demand(id) { FOLLOW_ON_DEMAND } else { 0 };
        // Only when the group says otherwise, as it is when the statement is added.
        let say = |now: FollowNow<'_>| {
            let (serial, at_s) = now.next;
            (now.counts.is_some_and(|fl| fl.on) != on).then_some(Body::Follow { target, target_pk: pk, name, on, serial, at_s, flags })
        };
        if add_follow(&ctx, &g, pk, &admitting(&f), say) {
            if on && let Some(e) = lock(&self.inner.cfg).my_devices.as_mut() {
                e.skip.retain(|s| *s != id.to_string());
            }
            crate::roles::changed(&ctx, &g).await;
        }
        Ok(())
    }

    /// Files on demand changed for folder `id` here: while it is on all the user's devices, the statement says so anew
    /// ([`FOLLOW_ON_DEMAND`]), so the devices that join it later get placeholders or every file. A device that has the
    /// folder already keeps its own mode. Only an owner or admin here says it (as when it was turned on).
    pub(crate) async fn follow_mode_changed(&self, id: &FolderId) {
        let ctx = self.inner.ctx.clone();
        let Some(g) = my_group(&ctx) else { return };
        let Some(f) = ctx.registry.user_space(id).filter(|f| f.kind == SpaceKind::Folder) else { return };
        if !f.role_of(ctx.key.public_key()).is_some_and(Role::admits) {
            return;
        }
        let pk = pk_of(&f);
        let want = if self.offers_on_demand(id) { FOLLOW_ON_DEMAND } else { 0 };
        let name = lock(&f.name).clone();
        // Only while it is on, and says otherwise, as the group is when the statement is added (never turning back on
        // one the user just turned off).
        let say = |now: FollowNow<'_>| {
            let flags = now.counts.filter(|fl| fl.on).map(|fl| fl.flags)?;
            if flags & FOLLOW_ON_DEMAND == want {
                return None;
            }
            let (serial, at_s) = now.next;
            Some(Body::Follow { target: FollowTarget::Folder, target_pk: pk, name, on: true, serial, at_s, flags: (flags & !FOLLOW_ON_DEMAND) | want })
        };
        if add_follow(&ctx, &g, pk, &admitting(&f), say) {
            crate::roles::changed(&ctx, &g).await;
        }
    }

    /// What the *On all my devices* statement of folder `id` says of files on demand ([`FOLLOW_ON_DEMAND`]): `Some` while
    /// the folder is on all the user's devices (whichever owner or admin of it said it last), `None` otherwise. What the hosts show
    /// as the folder's *Files on demand* ([`crate::status::FolderStatus::on_demand_others`]).
    pub fn follow_on_demand(&self, id: &FolderId) -> Option<bool> {
        let ctx = &self.inner.ctx;
        let f = ctx.registry.user_space(id).filter(|f| f.kind == SpaceKind::Folder)?;
        effective_follow(ctx, &f).filter(|fl| fl.on).map(|fl| fl.flags & FOLLOW_ON_DEMAND != 0)
    }

    /// Folder or channel `id` is on all the user's devices (the hosts ask before removing it).
    pub fn is_followed(&self, id: &FolderId) -> bool {
        self.inner.ctx.registry.user_space(id).is_some_and(|f| followed(&self.inner.ctx, &f))
    }

    /// Removing folder `id` here removes it from Seedwell on all the user's devices: it is on all of them and this
    /// device is its owner (the hosts say so before removing it). Their files stay where they are.
    pub fn removes_everywhere(&self, id: &FolderId) -> bool {
        let ctx = &self.inner.ctx;
        ctx.registry.user_space(id).is_some_and(|f| f.kind == SpaceKind::Folder && f.role_of(ctx.key.public_key()) == Some(Role::Owner) && followed(ctx, &f))
    }

    /// What [`Self::removed_everywhere`] needs of folder `id`, taken before it is removed: `None` when removing it here
    /// does not remove it on the other devices ([`Self::removes_everywhere`]). With the folder's owners and admins, whose
    /// Follows its statement ranks above (read while the folder's roles are still here).
    pub(crate) fn everywhere_follow(&self, id: &FolderId) -> Option<EverywhereFollow> {
        if !self.removes_everywhere(id) {
            return None;
        }
        let f = self.inner.ctx.registry.user_space(id)?;
        let name = lock(&f.name).clone();
        Some(EverywhereFollow { pk: pk_of(&f), name, admits: admitting(&f) })
    }

    /// Folder `ev.pk` was removed here by its owner: the group says so ([`FOLLOW_REMOVED`]), and every sibling removes
    /// it too, keeping its files. Its serial is taken as it is added ([`add_follow`]).
    pub(crate) async fn removed_everywhere(&self, ev: EverywhereFollow) {
        let ctx = self.inner.ctx.clone();
        let Some(g) = my_group(&ctx) else { return };
        let EverywhereFollow { pk, name, admits } = ev;
        let say = |now: FollowNow<'_>| {
            let (serial, at_s) = now.next;
            Some(Body::Follow { target: FollowTarget::Folder, target_pk: pk, name, on: false, serial, at_s, flags: FOLLOW_REMOVED })
        };
        if add_follow(&ctx, &g, pk, &admits, say) {
            crate::roles::changed(&ctx, &g).await;
        }
    }

    /// A folder or channel that is on all the user's devices is taken off this one (removed, or left): it does not come
    /// back by itself ("Not on This Mac"). Nothing for one that does not follow.
    pub(crate) fn skip_if_followed(&self, id: &FolderId) {
        if !self.is_followed(id) {
            return;
        }
        let changed = {
            let mut cfg = lock(&self.inner.cfg);
            match cfg.my_devices.as_mut() {
                Some(e) if !e.skip.contains(&id.to_string()) => {
                    e.skip.push(id.to_string());
                    true
                }
                _ => false,
            }
        };
        if changed {
            let _ = self.save_config();
        }
    }

    /// Brings what follows in line with the group: each folder and channel on all the user's devices is here (joined
    /// by itself, unless the user took it off this device), its siblings admitted where this device is owner or admin,
    /// one turned off that this device was only waiting for goes, and a folder its owner removed on all devices goes,
    /// its files kept (once: joined again afterwards, it stays).
    pub(crate) async fn devices_sync(&self) {
        let ctx = self.inner.ctx.clone();
        let _one = ctx.devices_lock.lock().await;
        match self.devices_key_sync().await {
            Ok(true) => {
                // The group changed (moved, or a new key): once more, on the group as it is now.
                ctx.devices_wake.notify_one();
                return;
            }
            Ok(false) => {}
            Err(e) => {
                if let Some(g) = ctx.registry.devices() {
                    g.problem(format!("your devices' key could not be changed: {e}"));
                }
            }
        }
        let Some(g) = my_group(&ctx) else { return };
        self.rendezvous_sync(&g).await;
        let me = *ctx.key.public_key();
        let (siblings, follows) = {
            let m = g.membership.read().unwrap_or_else(|e| e.into_inner());
            (group_members(&m, &me), m.follows().cloned().collect::<Vec<_>>())
        };
        let (skip, gone): (BTreeSet<String>, BTreeSet<String>) =
            self.group_entry().map(|e| (e.skip.into_iter().collect(), e.removed.into_iter().collect())).unwrap_or_default();
        // Once per folder: what 0.1.19's Read only lowered is given back, before this round's admissions.
        for f in ctx.registry.spaces().into_iter().filter(|s| s.kind == SpaceKind::Folder) {
            self.migrate_read_only(&f, &siblings).await;
        }
        for plain in follows {
            let id = FolderId::of_public_key(&plain.target_pk);
            let kind = match plain.target {
                FollowTarget::Folder => SpaceKind::Folder,
                FollowTarget::Channel => SpaceKind::Channel,
            };
            let here = ctx.registry.user_space(&id).filter(|f| f.kind == kind);
            // What counts for a folder or channel here is what its owners and admins said: a statement of a member that
            // is neither (one made Read only, say) turns nothing off and removes nothing.
            let fl = match &here {
                Some(f) => match effective_follow(&ctx, f) {
                    Some(fl) => {
                        self.restate_follow(&g, f).await;
                        fl
                    }
                    None => continue,
                },
                None => plain,
            };
            if kind == SpaceKind::Folder {
                if fl.removed() && !gone.contains(&id.to_string()) {
                    match &here {
                        // Its owner removed it on all devices: it goes from Seedwell here too, its files kept.
                        Some(f) if f.role_of(&fl.by) == Some(Role::Owner) => {
                            self.remove_removed(&g, &fl, &id).await;
                            continue;
                        }
                        // An admin's off statement marked removed: only an owner's removal reaches the other devices.
                        Some(_) => {}
                        // Here but not running: whose removal it is cannot be told, so nothing is removed.
                        None if self.configured(&id).is_some() => continue,
                        // Not here: nothing to remove; it is not joined again by itself.
                        None => {
                            self.note_removed(&id, true);
                            continue;
                        }
                    }
                }
                if fl.on && gone.contains(&id.to_string()) {
                    // On all devices again: a later removal is carried out again.
                    self.note_removed(&id, false);
                }
            }
            match here {
                Some(f) => {
                    let was = f.following.load(Ordering::SeqCst);
                    let now = mark_following(&ctx, &f);
                    if fl.on {
                        self.admit_siblings(&f, &siblings).await;
                    }
                    if now && !was {
                        self.offer_following(&f).await;
                    }
                    // Turned off while this device only waited for it: nothing of it is here, it goes.
                    if !fl.on && was && f.role_of(&me).is_none() && lock(&f.invite).is_none() {
                        let _ = match kind {
                            SpaceKind::Folder => self.remove_folder(&id).await.map(|_| ()),
                            _ => self.chat_leave(&id),
                        };
                    }
                }
                None if !fl.on || skip.contains(&id.to_string()) || self.configured(&id).is_some() => {}
                None if lock(&self.inner.cfg).channels.iter().any(|c| c.id == id.to_string()) => {}
                None => {
                    if let Err(e) = self.join_following(&fl).await {
                        g.problem(format!("“{}”, on all your devices, could not be added here: {e}", fl.name));
                    }
                }
            }
        }
    }

    /// Folder `id`'s owner removed it on all devices (`devices_sync` checked that its signer is an owner of the folder
    /// here, so a sibling that is not can never remove it, nor its key): it goes from Seedwell here too,
    /// its files kept where they are (an on-demand folder's become an ordinary folder at its place). Noted once done,
    /// so a folder joined again afterwards stays; a removal that fails (the folder busy) is tried again at the next
    /// change.
    async fn remove_removed(&self, g: &Space, fl: &Follow, id: &FolderId) {
        if self.inner.ctx.registry.user_space(id).is_some() || self.configured(id).is_some() {
            match self.remove_folder(id).await {
                Ok(at) => eprintln!("seedwell sync: “{}” was removed on all your devices; its files are in {}", fl.name, at.display()),
                Err(e) => {
                    g.problem(format!("“{}” was removed on all your devices, but could not be removed here yet: {e}", fl.name));
                    return;
                }
            }
        }
        self.note_removed(id, true);
    }

    /// Notes folder `id` as removed here because its owner removed it on all devices (`on`), or no longer.
    fn note_removed(&self, id: &FolderId, on: bool) {
        let changed = {
            let mut cfg = lock(&self.inner.cfg);
            let Some(e) = cfg.my_devices.as_mut() else { return };
            let had = e.removed.contains(&id.to_string());
            if on && !had {
                e.removed.push(id.to_string());
            } else if !on && had {
                e.removed.retain(|r| *r != id.to_string());
            }
            on != had
        };
        if changed {
            let _ = self.save_config();
        }
    }

    /// Shares the rendezvous (SPEC §7G.3) with the user's other devices: this device says in the group which one it
    /// uses (a [`Body::Rendezvous`] statement; empty once the user turned it off here), and a device that never had
    /// one takes the newest one another device says. Turning it off on one device turns it off there only: that device
    /// says "none" and takes none again by itself (what it said last is found in the groups it moved on from too).
    async fn rendezvous_sync(&self, g: &Arc<Space>) {
        let ctx = self.inner.ctx.clone();
        let me = *ctx.key.public_key();
        let url = self.rendezvous_for_paired().unwrap_or_default();
        let said = g.membership.read().unwrap_or_else(|e| e.into_inner()).rendezvous();
        let mine_here = said.iter().find(|(by, _, _)| *by == me).map(|(_, u, _)| u.clone());
        let mine = mine_here.clone().or_else(|| {
            ctx.registry.ancestors().iter().rev().find_map(|a| {
                a.membership.read().unwrap_or_else(|e| e.into_inner()).rendezvous().into_iter().find(|(by, _, _)| *by == me).map(|(_, u, _)| u)
            })
        });
        let say = if !url.is_empty() { Some(url.clone()) } else { mine.as_ref().map(|_| String::new()) };
        if let Some(say) = say.filter(|s| mine_here.as_ref() != Some(s)) {
            let serial = g.membership.read().unwrap_or_else(|e| e.into_inner()).next_rendezvous_serial(&me);
            let st = Statement::by_device(&ctx.key, &g.id, Body::Rendezvous { url: say, serial, at_s: now_s() });
            if crate::roles::add(g, [st]) {
                crate::roles::changed(&ctx, g).await;
            }
        }
        if url.is_empty() && mine.is_none() {
            let newest = said.iter().filter(|(by, u, _)| *by != me && !u.is_empty()).max_by_key(|(_, _, at)| *at).map(|(_, u, _)| u.clone());
            if let Some(u) = newest {
                match self.adopt_rendezvous_from_paired(&u).await {
                    Ok(true) => ctx.devices_wake.notify_one(),
                    Ok(false) => {}
                    Err(e) => g.problem(format!("the rendezvous your other devices use could not be taken: {e}")),
                }
            }
        }
    }

    /// Keeps the group's key safe (true when the group changed):
    /// 1. a current member moved the group (a new key after a removal): this device moves too, to the lowest group id
    ///    of the members' moves (two at once settle on one);
    /// 2. a group it moved on from turns out to have moved elsewhere too (two removals at once, met only now): of the
    ///    moves made by devices of its current group, the lowest group id wins, and this device moves there, taking
    ///    the devices removed in that earlier group out of the winner once it holds its key;
    /// 3. a device was removed (or left) and nobody moved the group yet: this device changes the key itself;
    /// 4. devices removed in an earlier group are still in this one: out, which changes the key again.
    async fn devices_key_sync(&self) -> Result<bool, SyncError> {
        let ctx = self.inner.ctx.clone();
        let Some(g) = ctx.registry.devices() else { return Ok(false) };
        let me = *ctx.key.public_key();
        if g.role_of(&me).is_none() || ctx.hold_key_changes.load(Ordering::SeqCst) {
            return Ok(false);
        }
        g.following.store(false, Ordering::SeqCst);
        let (moved, removed) = {
            let m = g.membership.read().unwrap_or_else(|e| e.into_inner());
            (m.moved_to(|_| true), m.removed_devices())
        };
        if let Some(t) = moved.filter(|t| FolderId::of_public_key(t) != g.id) {
            self.move_to(&g, t).await?;
            return Ok(true);
        }
        // Only moves made by devices of this group count here: a device minted with an old key is in none of them.
        let trusted = |pk: &[u8; 32]| g.role_of(pk).is_some();
        let ancestors = ctx.registry.ancestors();
        for (i, a) in ancestors.iter().enumerate() {
            // Where this device went from it (or later): the winner being one of those, nothing to do.
            let later: BTreeSet<FolderId> = ancestors[i + 1..].iter().map(|n| n.id).chain([g.id]).collect();
            let (winner, gone) = {
                let m = a.membership.read().unwrap_or_else(|e| e.into_inner());
                (m.moved_to(trusted), m.removed_devices())
            };
            if let Some(w) = winner.filter(|w| !later.contains(&FolderId::of_public_key(w))) {
                if let Some(e) = lock(&self.inner.cfg).my_devices.as_mut() {
                    e.pending_remove.extend(gone.iter().map(|pk| hex(pk)));
                    e.pending_remove.sort();
                    e.pending_remove.dedup();
                }
                self.move_to(&g, w).await?;
                return Ok(true);
            }
        }
        if !removed.is_empty() {
            self.rotate(&g).await?;
            return Ok(true);
        }
        let pending: Vec<String> = self.group_entry().map(|e| e.pending_remove).unwrap_or_default();
        if !pending.is_empty() && g.secret.read().unwrap_or_else(|e| e.into_inner()).is_some() {
            let out: Vec<[u8; 32]> = pending.iter().filter_map(|h| public_key_from_hex(h)).filter(|pk| *pk != me && g.role_of(pk).is_some()).collect();
            if let Some(e) = lock(&self.inner.cfg).my_devices.as_mut() {
                e.pending_remove.clear();
            }
            self.save_config()?;
            if !out.is_empty() {
                crate::roles::add_by_owner(&g, |_, serials| out.iter().map(|pk| Body::Remove { device: *pk, serial: serials.next(), at_s: now_s() }).collect());
                self.rotate(&g).await?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Admits `siblings` to `f` where this device may: an owner's grant as Admin for those not in it (and not removed
    /// from it by an owner), raising those at Read & write; an admin's one-use admission at Read & write.
    async fn admit_siblings(&self, f: &Arc<Space>, siblings: &[[u8; 32]]) {
        let ctx = &self.inner.ctx;
        let Some(adm) = admission(ctx, f) else { return };
        let added = if adm.owner {
            crate::roles::add_by_owner(f, |m, serials| {
                let pick = |d: &&[u8; 32]| admits_now(m, d) || m.role(d) == Some(Role::ReadWrite);
                siblings.iter().filter(pick).map(|d| adm.by_owner(*d, serials.next())).collect()
            }) == Some(true)
        } else {
            let new: Vec<[u8; 32]> = {
                let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
                siblings.iter().filter(|d| admits_now(&m, d)).copied().collect()
            };
            let sts: Vec<Statement> = new.into_iter().map(|d| adm.by_admin(ctx, f, d)).collect();
            !sts.is_empty() && crate::roles::add(f, sts)
        };
        if added {
            crate::roles::changed(ctx, f).await;
        }
    }

    /// Once per folder, after the update from 0.1.19 (whose *Read only* was a devices-group flag that lowered the user's
    /// other devices to Read only and raised them again): an owner here gives every sibling that design lowered the role
    /// it had before (the owners' decision before the lowering; Admin, as that design's off gave, for one first
    /// admitted under it), and when the old Read only still counts (the latest trusted Follow carries the flag), makes
    /// the folder Read only by the owners' setting ([`Body::ReadOnly`]), which changes no role. The old design is read
    /// from what it left: this owner's record (`devices_read_only`: when it came on; its `on` may be stale, the user
    /// having turned it off on another device meanwhile) or, without one, the group's Follows with the old flag. A
    /// sibling counts as lowered by it when it is Read only by a decision dated after the old run came on, as the old
    /// design's own off judged (these old times are the only clock compared, once), and that decision is one this
    /// device held when this version first opened the folder (`read_only_migrate_upto`): one made since (another
    /// owner's migration, the user's choice after it) stays. Noted as done (`read_only_migrated`) on any device; a
    /// device that is not an owner of the folder has nothing to give back.
    async fn migrate_read_only(&self, f: &Arc<Space>, siblings: &[[u8; 32]]) {
        let ctx = &self.inner.ctx;
        if f.kind != SpaceKind::Folder {
            return;
        }
        let key = f.id.to_string();
        let (legacy, upto) = match lock(&self.inner.cfg).folder(&key) {
            Some(e) if !e.read_only_migrated => (e.devices_read_only.clone(), e.read_only_migrate_upto.unwrap_or(u64::MAX)),
            _ => return,
        };
        let owner = f.role_of(ctx.key.public_key()) == Some(Role::Owner) && f.secret.read().unwrap_or_else(|e| e.into_inner()).is_some();
        let mut changed = false;
        if owner {
            let run = my_group(ctx).and_then(|g| legacy_run_start(&g, f));
            let came_on = match (&legacy, &run) {
                (Some(l), _) if l.on => Some(l.at_s),
                (_, Some(r)) => Some(r.at_s),
                _ => None,
            };
            if let Some(came_on) = came_on {
                let back = |m: &Membership, serials: &mut crate::roles::Serials| {
                    // Lowered by the old run: Read only by a decision dated after it came on, and made before this
                    // version first opened the folder (one above that is another owner's migration or the user's
                    // choice since, which stays).
                    let lowered = siblings.iter().filter(|d| {
                        m.members().any(|(pk, x)| pk == *d && x.role == Role::ReadOnly && x.since_s > came_on) && m.owner_serial(d).is_some_and(|s| s <= upto)
                    });
                    let before = |d: &[u8; 32]| m.owner_role_before_latest(d).filter(|r| matches!(r, Role::Admin | Role::ReadWrite)).unwrap_or(Role::Admin);
                    lowered.map(|d| Body::Grant { device: *d, role: before(d), serial: serials.next(), at_s: now_s(), invite: None }).collect()
                };
                changed |= crate::roles::add_by_owner(f, back) == Some(true);
            }
            // On only while the old flag still counts on the group (the latest trusted Follow carries it): this owner's
            // record says only when it came on, and a later off (said on another device while this one was away) wins.
            if run.is_some() {
                let on = |m: &Membership, serials: &mut crate::roles::Serials| if m.read_only() { Vec::new() } else { vec![Body::ReadOnly { on: true, serial: serials.next(), at_s: now_s() }] };
                changed |= crate::roles::add_by_owner(f, on) == Some(true);
            }
        }
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&key) {
            e.read_only_migrated = true;
            e.devices_read_only = None;
            e.read_only_migrate_upto = None;
        }
        if let Err(e) = self.save_config() {
            f.problem(format!("the folder's settings could not be saved: {e}"));
        }
        if changed {
            crate::roles::changed(ctx, f).await;
        }
    }

    /// The latest Follow for `f` of any of the user's devices (`plain`, what a device that does not have `f` yet, or
    /// runs an older Seedwell, goes by) is a member's that says something else than its owners and admins (`fl`): an
    /// owner or admin here says `fl` again, so such a device too joins it, or not, as they said (a sibling's "off" would
    /// keep a new device from joining). Only with a serial above it: never one statement after another. Both are read
    /// as the group is when the statement is added ([`add_follow`]).
    async fn restate_follow(&self, g: &Arc<Space>, f: &Space) {
        let ctx = &self.inner.ctx;
        let me = *ctx.key.public_key();
        let admits = admitting(f);
        if !admits.contains(&me) {
            return;
        }
        let pk = pk_of(f);
        let say = |now: FollowNow<'_>| {
            let (fl, plain) = (now.counts?, now.m.follow(&pk)?);
            if (plain.on, plain.flags) == (fl.on, fl.flags) || admits.contains(&plain.by) {
                return None;
            }
            let serial = now.m.next_follow_serial(&pk);
            (serial > plain.serial).then(|| Body::Follow { target: fl.target, target_pk: fl.target_pk, name: fl.name, on: fl.on, serial, at_s: now_s(), flags: fl.flags })
        };
        if add_follow(ctx, g, pk, &admits, say) {
            crate::roles::changed(ctx, g).await;
        }
    }

    /// A folder or channel that started following here is offered to the connected siblings (they admit it).
    async fn offer_following(&self, f: &Space) {
        let ctx = &self.inner.ctx;
        self.inner.wake_dialer.notify_one();
        for p in ctx.registry.peers() {
            if p.access(&f.id).is_some() {
                continue;
            }
            match f.kind {
                SpaceKind::Folder => {
                    if lock(&p.joined).contains(&f.id) {
                        continue;
                    }
                    let join = crate::peer::own_join(ctx, &[f.id], &p.exporter, &p.device_pk, p.lan).await;
                    if !join.folders.is_empty() {
                        lock(&p.joined).insert(f.id);
                        lock(&p.offered).insert(f.id);
                        let _ = p.control.send(Message::Join(join)).await;
                    }
                }
                SpaceKind::Channel if p.chat() => {
                    let join = crate::chat::own_chat_join(ctx, &[f.id], &p.exporter, &p.device_pk);
                    if !join.channels.is_empty() {
                        let _ = p.control.send(Message::ChatJoin(join)).await;
                    }
                }
                _ => {}
            }
        }
    }

    /// Joins a folder or channel one of the user's devices put on all of them: a folder at the default location for
    /// joined folders, under its name (with a number when that is taken: never into a folder that is there), on demand
    /// when it is on demand on the device that put it there and this device keeps folders on demand (its place there,
    /// its files in this device's cache), else in full (saying why on its page when it was on demand there); a channel
    /// under its name.
    async fn join_following(&self, fl: &Follow) -> Result<(), SyncError> {
        let id = FolderId::of_public_key(&fl.target_pk);
        match fl.target {
            FollowTarget::Channel => {
                let name = Some(fl.name.trim().to_string()).filter(|n| !n.is_empty() && !n.chars().any(char::is_control)).unwrap_or_else(|| "Channel".into());
                let entry = ChannelEntry { id: id.to_string(), channel_pk: hex(&fl.target_pk), name, ..ChannelEntry::default() };
                {
                    let mut cfg = lock(&self.inner.cfg);
                    cfg.channels.retain(|c| c.id != entry.id);
                    cfg.channels.push(entry.clone());
                }
                self.save_config()?;
                self.open_channel(&entry).await
            }
            FollowTarget::Folder => {
                let base = self.default_location()?;
                if in_real_home_under_test(&base) {
                    return Err(invalid(format!("refusing to make a folder in {} inside the real home directory while running under test", base.display())));
                }
                std::fs::create_dir_all(&base)?;
                let name = folder_name(&fl.name).unwrap_or_else(|| id.to_string().split('-').next().unwrap_or("Folder").to_string());
                let places: Vec<PathBuf> = lock(&self.inner.cfg).folders.iter().map(|e| e.path.clone()).collect();
                let path = unique_in(&base, &name, |p| places.iter().any(|q| p.starts_with(q) || q.starts_with(p)));
                let on_demand = fl.on_demand() && self.inner.on_demand;
                self.join_following_at(fl.target_pk, path, on_demand).await?;
                if fl.on_demand() && !on_demand {
                    self.note_full_here(&id);
                }
                Ok(())
            }
        }
    }

    /// **Tests:** this device says in its group, past every check (as a sibling on another build could), that folder
    /// `id` follows (`on`) with `flags`, at `serial` (`None`: above every one it knows, as a build that counts every
    /// member's would). True when the group took it.
    pub async fn test_say_follow(&self, id: &FolderId, on: bool, flags: u64, serial: Option<u64>) -> bool {
        let ctx = self.inner.ctx.clone();
        let (Some(g), Some(f)) = (my_group(&ctx), ctx.registry.user_space(id)) else { return false };
        let pk = pk_of(&f);
        let serial = serial.unwrap_or_else(|| g.membership.read().unwrap_or_else(|e| e.into_inner()).next_follow_serial(&pk));
        let name = lock(&f.name).clone();
        let st = Statement::by_device(&ctx.key, &g.id, Body::Follow { target: FollowTarget::Folder, target_pk: pk, name, on, serial, at_s: now_s(), flags });
        let took = crate::roles::add(&g, [st]);
        if took {
            crate::roles::changed(&ctx, &g).await;
        }
        took
    }

    /// **Tests:** this device's group holds a Follow for folder `id` said by `by` with `flags` (it has received it,
    /// whether it counts or not).
    pub fn test_has_follow(&self, id: &FolderId, by: &DeviceId, flags: u64) -> bool {
        let ctx = &self.inner.ctx;
        let (Some(g), Some(f)) = (my_group(ctx), ctx.registry.user_space(id)) else { return false };
        let pk = pk_of(&f);
        g.membership.read().unwrap_or_else(|e| e.into_inner()).follows_by(&pk, |_| true).iter().any(|fl| DeviceId::of_public_key(&fl.by) == *by && fl.flags == flags)
    }

    /// **Tests:** the statements of folder `id`'s membership here.
    pub fn test_statements(&self, id: &FolderId) -> Vec<Statement> {
        let Some(f) = self.inner.ctx.registry.user_space(id) else { return Vec::new() };
        f.membership.read().unwrap_or_else(|e| e.into_inner()).statements().cloned().collect()
    }

    /// **Tests:** adds `statements` to folder `id`'s membership here and nothing else (as a session delivers them,
    /// before this device acts on them). True when it changed.
    pub fn test_add_statements(&self, id: &FolderId, statements: Vec<Statement>) -> bool {
        self.inner.ctx.registry.user_space(id).is_some_and(|f| crate::roles::add(&f, statements))
    }

    /// **Tests:** the statements of this device's group.
    pub fn test_group_statements(&self) -> Vec<Statement> {
        let Some(g) = my_group(&self.inner.ctx) else { return Vec::new() };
        g.membership.read().unwrap_or_else(|e| e.into_inner()).statements().cloned().collect()
    }

    /// **Tests:** takes `statements` into this device's group as a session delivers them (and acts on them).
    pub async fn test_take_group_statements(&self, statements: Vec<Statement>) -> bool {
        let ctx = self.inner.ctx.clone();
        let Some(g) = my_group(&ctx) else { return false };
        let took = crate::roles::add(&g, statements);
        if took {
            crate::roles::changed(&ctx, &g).await;
        }
        took
    }

    /// **Tests:** this device's folder `id` holds back from writing ([`held_read_only`]).
    pub fn test_held_read_only(&self, id: &FolderId) -> bool {
        self.inner.ctx.registry.user_space(id).is_some_and(|f| f.held_read_only.load(Ordering::SeqCst))
    }

    /// **Tests:** the id of this device's devices group.
    pub fn test_group_id(&self) -> Option<FolderId> {
        self.inner.ctx.registry.devices().map(|g| g.id)
    }

    /// **Tests:** a thief's device: signs `other` into this device's group with the user key this device holds (also
    /// after it was removed), and gives `other` the group as it is here, the key and the devices met in it. True when
    /// it could.
    pub async fn test_mint_into_group(&self, other: &SyncHandle) -> bool {
        let Some(g) = self.inner.ctx.registry.devices() else { return false };
        let pk = *other.inner.ctx.key.public_key();
        let serial = g.membership.read().unwrap_or_else(|e| e.into_inner()).next_serial();
        let Some(st) = crate::roles::by_owner(&g, Body::Grant { device: pk, role: Role::Owner, serial, at_s: now_s(), invite: None }) else { return false };
        let mut m = Membership::new_devices(pk_of(&g));
        m.add(g.membership.read().unwrap_or_else(|e| e.into_inner()).statements().cloned().chain([st]).collect::<Vec<_>>());
        let Some(secret) = g.secret.read().unwrap_or_else(|e| e.into_inner()).clone() else { return false };
        if crate::roles::save(&other.members_path(&g.id), &m).is_err() || other.inner.secrets.put(&crate::roles::secret_name(&g.id), secret.as_bytes()).is_err() {
            return false;
        }
        let devices = self.group_entry().map(|e| e.devices).unwrap_or_default();
        let entry = MyDevicesEntry { id: g.id.to_string(), group_pk: hex(&pk_of(&g)), devices, mode: "full".into(), ..MyDevicesEntry::default() };
        lock(&other.inner.cfg).my_devices = Some(entry.clone());
        other.save_config().is_ok() && other.open_devices(&entry).await.is_ok()
    }

    /// **Tests:** while `on`, this device leaves its group's key as it is: it neither moves to another group nor makes
    /// a new key, whatever it learns (off: it catches up at once).
    pub fn test_hold_key_changes(&self, on: bool) {
        self.inner.ctx.hold_key_changes.store(on, Ordering::SeqCst);
        self.inner.ctx.devices_wake.notify_one();
    }

    /// **Tests:** the highest owner serial of folder `id`'s membership here (0: none, or no such folder).
    pub fn test_max_serial(&self, id: &FolderId) -> u64 {
        self.inner.ctx.registry.user_space(id).map_or(0, |f| f.membership.read().unwrap_or_else(|e| e.into_inner()).max_serial())
    }

    /// **Tests:** folder `id` as an update from 0.1.19 leaves it: its *Read only* migration not run yet, with what 0.1.19
    /// noted of its Read only (`legacy`: on, and when it came on), the highest owner serial it held when this version
    /// first opened it (`upto`; `None`: what it holds now), and a round of the user's devices to run it.
    pub fn test_forget_read_only_migration(&self, id: &FolderId, legacy: Option<(bool, u64)>, upto: Option<u64>) {
        let now = self.inner.ctx.registry.user_space(id).map(|f| f.membership.read().unwrap_or_else(|e| e.into_inner()).max_serial());
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            e.read_only_migrated = false;
            e.devices_read_only = legacy.map(|(on, at_s)| crate::config::LegacyReadOnly { on, at_s });
            e.read_only_migrate_upto = upto.or(now);
        }
        self.inner.ctx.devices_wake.notify_one();
    }

    /// **Tests:** while `on`, a switch of this device to another group waits once that group and the ones it moved on
    /// from are ready, before they are in use ([`Self::test_switches_waiting`]).
    pub fn test_hold_switch(&self, on: bool) {
        self.inner.ctx.hold_switch.store(on, Ordering::SeqCst);
    }

    /// **Tests:** switches waiting in [`Self::test_hold_switch`]'s wait now.
    pub fn test_switches_waiting(&self) -> usize {
        self.inner.ctx.switches_waiting.load(Ordering::SeqCst)
    }

    /// **Tests:** this device changes its group's key now, as after a removal, whatever it knows of moves (as a
    /// device does that decided to before it learned of another's); with `below`, to a group whose id is lower than
    /// that one. The new group's id.
    pub async fn test_rotate(&self, below: Option<FolderId>) -> Result<FolderId, SyncError> {
        let ctx = self.inner.ctx.clone();
        let _one = ctx.devices_lock.lock().await;
        let g = my_group(&ctx).ok_or(SyncError::NotFound)?;
        let secret = loop {
            let s = FolderSecret::generate(&mut ring::rand::SystemRandom::new()).map_err(io::Error::other)?;
            if below.is_none_or(|b| FolderId::of_public_key(s.public_key()) < b) {
                break s;
            }
        };
        let id = FolderId::of_public_key(secret.public_key());
        self.rotate_to(&g, secret).await?;
        Ok(id)
    }

    /// **Tests:** this device holds the user key (in memory and in its secret store).
    pub fn test_has_user_key(&self) -> bool {
        let Some(g) = self.inner.ctx.registry.devices() else { return false };
        let held = g.secret.read().unwrap_or_else(|e| e.into_inner()).is_some();
        held && self.inner.secrets.get(&crate::roles::secret_name(&g.id)).ok().flatten().is_some()
    }

    /// The default location for joined folders (Settings; empty: `Seedwell` in the home folder).
    fn default_location(&self) -> Result<PathBuf, SyncError> {
        let set = lock(&self.inner.cfg).settings.default_location.trim().to_string();
        if !set.is_empty() {
            return Ok(PathBuf::from(set));
        }
        seedwell_platform::dirs::home().map(|h| h.join("Seedwell")).ok_or_else(|| invalid("no default location for joined folders is set"))
    }
}

/// Under test (a cargo test binary, or `SEEDWELL_FORBID_REAL_HOME`; `SEEDWELL_ALLOW_REAL_HOME` overrides, the engine's rule):
/// `path` is inside the real home folder (the temporary folder excepted), where nothing is made by itself.
fn in_real_home_under_test(path: &Path) -> bool {
    let test_binary = std::env::current_exe().ok().and_then(|p| p.parent().and_then(|d| d.file_name()).map(|n| n == "deps")).unwrap_or(false);
    let guarded = (cfg!(test) || test_binary || std::env::var_os("SEEDWELL_FORBID_REAL_HOME").is_some()) && std::env::var_os("SEEDWELL_ALLOW_REAL_HOME").is_none();
    guarded
        && seedwell_platform::dirs::home().filter(|h| h.is_absolute() && h.parent().is_some()).is_some_and(|h| path.starts_with(&h))
        && !seedwell_platform::dirs::in_temp_folder(path)
}

/// A folder's name from a Follow's name: one path component, or `None`.
fn folder_name(name: &str) -> Option<String> {
    let n: String = seedwell_platform::fs::clean_name(name).chars().take(100).collect();
    let n = n.trim().trim_start_matches('.').trim().to_owned();
    (!n.is_empty()).then_some(n)
}

/// `dir/name`, or `dir/name 2`, `dir/name 3`, … while it is taken (there on disk, or `taken`: another folder's place,
/// which an on-demand folder's is without being on disk): a folder that follows is never put into one that is there.
fn unique_in(dir: &Path, name: &str, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let free = |p: &Path| std::fs::symlink_metadata(p).is_err() && !taken(p);
    let first = dir.join(name);
    if free(&first) {
        return first;
    }
    for i in 2..10_000 {
        let p = dir.join(format!("{name} {i}"));
        if free(&p) {
            return p;
        }
    }
    dir.join(format!("{name} {}", std::process::id()))
}

fn take_request(ctx: &Ctx, device: &DeviceId) -> Option<PairRequest> {
    let mut p = lock(&ctx.pairing);
    let i = p.iter().position(|r| DeviceId::of_public_key(&r.device_pk) == *device)?;
    Some(p.remove(i))
}

/// Runs [`SyncHandle::devices_sync`] whenever something the devices group depends on changed.
pub(crate) async fn run(h: SyncHandle) {
    loop {
        h.inner.ctx.devices_wake.notified().await;
        if h.is_stopping() {
            return;
        }
        h.devices_sync().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_test_nothing_is_made_in_the_real_home_by_itself() {
        if std::env::var_os("SEEDWELL_ALLOW_REAL_HOME").is_some() {
            return;
        }
        let Some(home) = seedwell_platform::dirs::home() else { return };
        assert!(in_real_home_under_test(&home.join("Seedwell")), "the default ~/Seedwell is refused under test");
        assert!(!in_real_home_under_test(&std::env::temp_dir().join("x")), "a temporary folder is not");
    }

    #[test]
    fn a_folder_that_follows_never_lands_in_one_that_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let none = |_: &Path| false;
        assert_eq!(unique_in(dir.path(), "Dev", none), dir.path().join("Dev"));
        std::fs::create_dir(dir.path().join("Dev")).unwrap();
        std::fs::create_dir(dir.path().join("Dev 2")).unwrap();
        assert_eq!(unique_in(dir.path(), "Dev", none), dir.path().join("Dev 3"));
        // An on-demand folder's place is not on disk, and still taken.
        let place = dir.path().join("Dev 3");
        assert_eq!(unique_in(dir.path(), "Dev", |p| p == place), dir.path().join("Dev 4"));
        let n = folder_name("  ../Dev/x  ").unwrap();
        assert!(!n.contains('/') && !n.starts_with('.'), "one plain component: {n}");
        assert_eq!(folder_name("   "), None);
    }
}
