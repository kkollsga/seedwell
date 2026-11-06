//! A folder's or a channel's membership on this device (SPEC §7G.1): kept in `<id>.members` next to the index,
//! proved when connecting, applied when it changes. Everything here works on a [`Space`], so folders and channels
//! share it.
//!
//! When the statements change, [`changed`] brings everything in line: a folder's writer table and this device's
//! access, the sessions (a device that is no longer a member is cut off, the others get the new statements),
//! and an owner's duties (signing again what admins admitted, giving the secret to co-owners).

use std::io;
use std::path::Path;
use std::sync::Arc;

use trs_sync_proto::codec::{Reader, put_bytes, put_varint};
use trs_sync_proto::ids::{DeviceId, ShortId};
use trs_sync_proto::keys::{Access, FolderSecret, Proof};
use trs_sync_proto::members::{Body, INVITE_VERSION, Membership, Role, Statement, check_redeem, redeem_proof};
use trs_sync_proto::msg::{Members, Message, OwnerKey};

use crate::shared::{Ctx, Space, SpaceKind, lock};

/// A membership file: `MAGIC ‖ format varint ‖ n × (varint length ‖ statement)`. Statements are kept as they
/// came (also kinds this version does not know), so passing them on later loses nothing.
const MAGIC: &[u8] = b"SWMB";
/// The layout of the membership file this version writes and reads.
const MEMBERS_FORMAT: u64 = 1;
/// What the test builds before 0.1.6 wrote.
const OLD_MAGIC: &[u8] = b"SWM2";

/// The secret store's name for a folder's (or channel's) secret `S` (owners only).
pub(crate) fn secret_name(id: &trs_sync_proto::ids::FolderId) -> String {
    format!("folder-{id}")
}

/// The secret store's name for the invitation this device joins a folder with.
pub(crate) fn invite_name(id: &trs_sync_proto::ids::FolderId) -> String {
    format!("invite-{id}")
}

/// The secret store's name for the links of the invitations this device made for a folder (one per line).
pub(crate) fn links_name(id: &trs_sync_proto::ids::FolderId) -> String {
    format!("links-{id}")
}

pub(crate) fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The membership file was written by a test build before 0.1.6 (its folder is forgotten at start).
pub(crate) fn from_test_build(path: &Path) -> bool {
    use std::io::Read as _;
    let mut head = [0u8; 4];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && head == OLD_MAGIC
}

/// Reads a folder's membership; a missing file is an empty one.
pub(crate) fn load(path: &Path, folder_pk: [u8; 32]) -> io::Result<Membership> {
    load_into(path, Membership::new(folder_pk))
}

/// Reads a devices group's membership ([`Membership::new_devices`]); a missing file is an empty one.
pub(crate) fn load_devices(path: &Path, group_pk: [u8; 32]) -> io::Result<Membership> {
    load_into(path, Membership::new_devices(group_pk))
}

fn load_into(path: &Path, mut m: Membership) -> io::Result<Membership> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(m),
        Err(e) => return Err(e),
    };
    let damaged = |why: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {why}", path.display()));
    if bytes.starts_with(OLD_MAGIC) {
        return Err(damaged("written by a test build of Seedwell: remove the folder and join it again"));
    }
    let body = bytes.strip_prefix(MAGIC).ok_or_else(|| damaged("not a membership file"))?;
    let mut r = Reader::new(body);
    match r.varint("format").map_err(|e| damaged(&e.to_string()))? {
        MEMBERS_FORMAT => {}
        n => return Err(damaged(&format!("written by a newer Seedwell (format {n}): update Seedwell"))),
    }
    let mut statements = Vec::new();
    while r.remaining() > 0 {
        let b = r.bytes(64 * 1024, "statement").map_err(|e| damaged(&e.to_string()))?;
        statements.push(Statement::from_bytes(b).map_err(|e| damaged(&e.to_string()))?);
    }
    m.add(statements);
    m.set_now(now_s());
    Ok(m)
}

/// Writes a folder's membership atomically.
pub(crate) fn save(path: &Path, m: &Membership) -> io::Result<()> {
    write_bytes(path, &bytes(m))
}

/// A membership as its file keeps it ([`write_bytes`] writes it).
fn bytes(m: &Membership) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    put_varint(&mut out, MEMBERS_FORMAT);
    for st in m.statements() {
        put_bytes(&mut out, &st.to_bytes());
    }
    out
}

/// Writes a membership's [`bytes`] to `path` atomically.
pub(crate) fn write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("members.tmp");
    std::fs::write(&tmp, bytes)?;
    crate::shared::slow_disk();
    std::fs::rename(&tmp, path)
}

/// "folder" or "channel", for messages.
pub(crate) fn noun(f: &Space) -> &'static str {
    match f.kind {
        SpaceKind::Folder => "folder",
        SpaceKind::Channel => "channel",
        SpaceKind::Devices => "group of your devices",
    }
}

/// Adds statements to a membership and keeps it on disk. True when roles or invitations changed (call [`changed`]
/// then).
pub(crate) fn add(f: &Space, statements: impl IntoIterator<Item = Statement>) -> bool {
    {
        let mut m = f.membership.write().unwrap_or_else(|e| e.into_inner());
        let ended = m.set_now(now_s());
        if !m.add(statements) {
            return ended;
        }
    }
    keep(f);
    true
}

/// The serials of the statements [`add_by_owner`] signs: each [`Serials::next`] is the next free one.
pub(crate) struct Serials(u64);

impl Serials {
    pub(crate) fn next(&mut self) -> u64 {
        let s = self.0;
        self.0 += 1;
        s
    }
}

/// Signs with `f`'s owner key what `make` decides, on the membership as it is, and adds it, all under one write lock:
/// `make` gets the membership and the serials to give (above every owner statement in it), and returns the bodies.
/// Owner statements made at once (a confirmation of an admin's admission and the user's role change, say) never share
/// a serial, and none is decided on an older look at the membership and added after a newer decision (which reset a
/// device just made an admin to Read only). `make` takes no lock (the membership's is held); the file is
/// saved after, outside it. `None`: the owner key is not on this device; else as [`add`].
pub(crate) fn add_by_owner(f: &Space, make: impl FnOnce(&Membership, &mut Serials) -> Vec<Body>) -> Option<bool> {
    let secret = f.secret.read().unwrap_or_else(|e| e.into_inner()).clone()?;
    {
        let mut m = f.membership.write().unwrap_or_else(|e| e.into_inner());
        let ended = m.set_now(now_s());
        let mut serials = Serials(m.next_serial());
        let bodies = make(&m, &mut serials);
        #[cfg(test)]
        tests::decided(&f.id);
        let statements: Vec<Statement> = bodies.into_iter().map(|b| Statement::by_owner(&secret, b)).collect();
        if statements.is_empty() || !m.add(statements) {
            return Some(ended);
        }
    }
    keep(f);
    Some(true)
}

/// Writes a space's membership to its file. The membership is locked only while its bytes are made, never for the
/// write: the app's calls read it, and a disk may take long to save (a virus scanner reads each new file first). The
/// saves take turns ([`Space::saving`]), and each writes the membership as it is at its turn, so the file never goes
/// back to an older one.
pub(crate) fn keep(f: &Space) {
    let _turn = lock(&f.saving);
    let b = bytes(&f.membership.read().unwrap_or_else(|e| e.into_inner()));
    if let Err(e) = write_bytes(&f.members_path, &b) {
        f.problem(format!("the {}'s membership could not be saved: {e}", noun(f)));
    }
}

/// The sharing ended (or counts again) since the last look: true then (call [`changed`]).
pub(crate) fn tick(f: &Space) -> bool {
    f.membership.write().unwrap_or_else(|e| e.into_inner()).set_now(now_s())
}

/// Signs `body` with the folder (or channel) key, on an owner device. Only for a statement that takes no owner serial
/// (an invitation, a revocation): a grant, a removal or a share end goes through [`add_by_owner`].
pub(crate) fn by_owner(f: &Space, body: Body) -> Option<Statement> {
    let secret = f.secret.read().unwrap_or_else(|e| e.into_inner()).clone()?;
    Some(Statement::by_owner(&secret, body))
}

/// Signs this device's name and its user's into the membership when it is a member there and they differ from what
/// it published (or it published none): every member then shows them, also one that never met this device. True
/// when it added one (call [`changed`] then, so the sessions get it).
pub(crate) fn publish_profile(ctx: &Ctx, f: &Space) -> bool {
    use trs_sync_proto::members::profile_name_ok;
    let me = *ctx.key.public_key();
    let name = lock(&ctx.device_name).clone();
    let user = lock(&ctx.user_name).clone();
    if !profile_name_ok(&name, false) || !profile_name_ok(&user, true) {
        return false;
    }
    // Not into a devices group the device moved on from.
    if f.kind == SpaceKind::Devices && ctx.registry.devices().map(|g| g.id) != Some(f.id) {
        return false;
    }
    let serial = {
        let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
        if m.role(&me).is_none() || m.profile(&me) == Some((name.as_str(), user.as_str())) {
            return false;
        }
        m.next_profile_serial(&me)
    };
    let st = Statement::by_device(&ctx.key, &f.id, Body::Profile { device_name: name, user_name: user, serial, at_s: now_s() });
    add(f, [st])
}

/// The membership changed: bring a folder's core, the sessions and this device's duties in line.
/// The short ids of every device that counts in the folder now (any role).
pub(crate) fn member_ids(m: &Membership) -> std::collections::BTreeSet<ShortId> {
    m.members().map(|(pk, _)| DeviceId::of_public_key(pk).short()).collect()
}

/// An owner signs again what admins admitted (same role, same invitation, same time: [`confirmations`]), so those
/// devices stay if that admin is later demoted. True when it signed any. Nothing on a device that is not an owner.
pub(crate) fn confirm_admitted(ctx: &Ctx, f: &Space) -> bool {
    if f.role_of(ctx.key.public_key()) != Some(Role::Owner) {
        return false;
    }
    add_by_owner(f, confirmations) == Some(true)
}

/// The owner's statements that sign again what admins admitted in `m`: per member whose role rests on an admin's
/// grant only, the same role and invitation, dated when the admin admitted it, not when the owner signs: confirming
/// decides nothing new, so the member's time stays. Dated by the owner's clock, a device an admin admitted Read only
/// under *Read only* (dated a second after it came on) and confirmed in the second it came on (or by an owner whose
/// clock is behind) counted as Read only from before, and turning it off left it Read only for good.
fn confirmations(m: &Membership, serials: &mut Serials) -> Vec<Body> {
    let since: std::collections::BTreeMap<[u8; 32], u64> = m.members().map(|(pk, mm)| (*pk, mm.since_s)).collect();
    m.to_confirm()
        .into_iter()
        .map(|(device, role, invite)| {
            let at_s = since.get(&device).copied().unwrap_or_else(now_s);
            Body::Grant { device, role, serial: serials.next(), at_s, invite }
        })
        .collect()
}

/// Device `me` holds its changes to the folder of membership `m` back (SPEC §7G.1 *Read only*): the folder is Read only
/// and `me` is a member that could write but is not an owner. Its role stays as it is; only the owners' versions count
/// meanwhile ([`Membership::writers`]).
pub(crate) fn held_read_only(m: &Membership, me: &[u8; 32]) -> bool {
    m.read_only() && m.role(me).is_some_and(|r| r != Role::Owner && r.access() == Access::Write)
}

/// Tells folder core `core` whether this device holds its changes back (`held`, [`held_read_only`]), before its
/// membership is applied: held, the core notes it, so once it signs again (here, or at the next start) what it changed
/// meanwhile becomes its own versions; not held and not signing by its `role` (it reads only, or was removed), it
/// forgets it (a reader's changes stay local differences). Otherwise the core clears it itself as it signs again.
pub(crate) fn core_held(core: &mut trs_sync::folder::FolderCore, held: bool, role: Option<Role>) -> io::Result<()> {
    if held {
        core.set_held(true)
    } else if role.is_none_or(|r| r.access() == Access::Read) {
        core.set_held(false)
    } else {
        Ok(())
    }
}

/// An owner's *Read only* setting `on` for the folder of membership `m`: its statement, when that is not what the
/// folder says already ([`add_by_owner`] gives the serial).
pub(crate) fn read_only_statement(m: &Membership, serials: &mut Serials, on: bool) -> Vec<Body> {
    if m.read_only() == on { Vec::new() } else { vec![Body::ReadOnly { on, serial: serials.next(), at_s: now_s() }] }
}

pub(crate) async fn changed(ctx: &Arc<Ctx>, f: &Arc<Space>) {
    let me = *ctx.key.public_key();
    // Admitted (or not named here yet): the others learn this device's name with the statements below.
    publish_profile(ctx, f);
    confirm_admitted(ctx, f);
    // Removals this device asked for that the owners carried out are done.
    crate::removals::settle(f);
    // The folder is Read only (its owners' setting) and this device is not an owner: it holds its changes back.
    let held = f.kind == SpaceKind::Folder && held_read_only(&f.membership.read().unwrap_or_else(|e| e.into_inner()), &me);
    f.held_read_only.store(held, std::sync::atomic::Ordering::SeqCst);
    let (role, statements) = {
        let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
        (m.role(&me), m.statements().cloned().collect::<Vec<_>>())
    };

    // A folder's core: who writes, and whether this device does.
    if let Some(folder) = ctx.registry.folder(&f.id) {
        let (f2, space) = (folder.clone(), f.clone());
        let set = tokio::task::spawn_blocking(move || {
            let mut core = lock(&f2.core);
            // The membership as it is now, read under the core's lock: calls of this function overlap, and one that
            // read it earlier must not apply an older membership after a newer one (a device just admitted would be
            // told it may only read again, and its edits kept as a reader's).
            let (role, writers, members, owners, held) = {
                let m = space.membership.read().unwrap_or_else(|e| e.into_inner());
                (m.role(&me), m.writers(), member_ids(&m), m.read_only().then(|| m.owners()), held_read_only(&m, &me))
            };
            let access = if held { Access::Read } else { role.map_or(Access::Read, Role::access) };
            // Read only: only the owners' versions count here.
            core.set_read_only(owners.as_deref()).map_err(|e| e.to_string())?;
            core_held(&mut core, held, role).map_err(|e| e.to_string())?;
            // Joining with write access, not admitted yet: what it changes meanwhile is its own once it is.
            let awaiting = role.is_none() && lock(&space.invite).as_ref().is_some_and(|i| i.invite.role.access() == Access::Write);
            let signed = core.signs();
            // With the flag as it was while it waited: admitted now, its changes made meanwhile become its versions.
            core.set_membership(&writers, access).map_err(|e| e.to_string())?;
            core.set_awaiting_write(awaiting);
            // A device no longer a member (removed, or its access ended with the sharing) is no source or holder any
            // more (its entries stay: which version is current never changes). Only as a member: one still waiting to
            // be admitted may not know them all yet.
            if role.is_some() {
                core.set_members(&members).map_err(|e| e.to_string())?;
                lock(&f2.items).set_not_members(core.not_members());
            }
            Ok::<_, String>((core.index_state().map(|(_, s)| s).unwrap_or(0), !signed && core.signs()))
        })
        .await;
        match set {
            // Allowed to write now, its files only it had became its own: the sessions announce them, and a scan
            // reads the changes it made while it waited into its own versions.
            Ok(Ok((seq, began_signing))) => {
                folder.local_seq.send_if_modified(|s| std::mem::replace(s, seq) != seq);
                if began_signing {
                    folder.rescan.notify_one();
                }
            }
            Ok(Err(e)) => f.problem(format!("the folder's membership could not be applied: {e}")),
            Err(_) => {}
        }
    }
    // Admitted: the invitation has done its job.
    if role.is_some() && lock(&f.invite).take().is_some() {
        let _ = ctx.secrets.delete(&invite_name(&f.id));
        lock(&f.notes).refused = None;
    }

    // Sessions: a device that is no longer a member is cut off; every other one gets the statements, and a
    // co-owner the folder secret.
    let secret = f.secret.read().unwrap_or_else(|e| e.into_inner()).clone();
    for p in ctx.registry.peers() {
        if p.access(&f.id).is_none() {
            continue;
        }
        match f.role_of(&p.device_pk) {
            None => {
                // Told first, so it knows; then the session ends (a channel only stops being shared over it).
                let _ = p.control.send(Message::Members(Members { folder_id: f.id, statements: statements.clone() })).await;
                // A channel or the devices group only stops being shared over it (the session may carry folders).
                if f.kind != SpaceKind::Folder {
                    p.folders.write().unwrap_or_else(|e| e.into_inner()).remove(&f.id);
                    continue;
                }
                let ended = f.membership.read().unwrap_or_else(|e| e.into_inner()).expired(&p.device_pk);
                let reason = if ended { "this folder is no longer shared" } else { "this device was removed from a folder" }.to_string();
                let _ = p.control.send(Message::Close(trs_sync_proto::msg::Close { reason })).await;
            }
            // A device this one (an admin) asked the owners to remove: cut off here until their removal comes.
            Some(_) if crate::removals::waiting(f, &p.device_pk) => crate::removals::cut_off(f, &p).await,
            Some(r) => {
                crate::removals::send_waiting(f, &p).await;
                p.folders.write().unwrap_or_else(|e| e.into_inner()).insert(f.id, r.access());
                let _ = p.control.send(Message::Members(Members { folder_id: f.id, statements: statements.clone() })).await;
                if r == Role::Owner
                    && let Some(s) = &secret
                {
                    let _ = p.control.send(Message::OwnerKey(OwnerKey { folder_id: f.id, secret: *s.as_bytes() })).await;
                }
            }
        }
    }
    // The devices group, or a folder or channel that may follow in it: what follows is brought in line.
    if f.kind == SpaceKind::Devices || ctx.registry.devices().is_some() {
        ctx.devices_wake.notify_one();
    }
}

/// This device's proofs for a folder (or channel) in a Join, for the device `peer_pk`: its grant (and the admin's
/// that signed it), or the invitation it joins with, bound to the session, or, following one of the user's own
/// devices and talking to one of them, that it is one of them ([`Proof::Sibling`]). `None`: nothing to prove
/// (removed, or never a member).
pub(crate) fn proofs(ctx: &Ctx, f: &Space, exporter: &[u8; 32], peer_pk: &[u8; 32]) -> Option<Vec<Proof>> {
    let me = ctx.key.public_key();
    let grants = f.membership.read().unwrap_or_else(|e| e.into_inner()).proofs_for(me);
    if !grants.is_empty() {
        return Some(grants.iter().map(|st| Proof::Grant(st.to_bytes())).collect());
    }
    if let Some(invite) = lock(&f.invite).clone() {
        return Some(vec![Proof::Invite(redeem_proof(&invite.invite, &invite.token_seed, me, exporter))]);
    }
    // A devices group this device moves to (its key changed after a removal): nothing to prove; its members know
    // this device's key from the group it moved from.
    if f.kind == SpaceKind::Devices {
        return f.following.load(std::sync::atomic::Ordering::SeqCst).then(Vec::new);
    }
    crate::devices::sibling_proof(ctx, f, peer_pk).map(|p| vec![p])
}

/// What a peer's Join proves for one folder.
pub(crate) enum Proved {
    /// A member, with this access.
    Member(Access),
    /// It redeemed an invitation that this device refused, for this reason (told to the peer).
    Refused(String),
    /// It redeemed a pairing link of this device's: waiting for the user here to confirm (the devices group only).
    Pending,
    /// Nothing this device can accept (a joining device met a member that does not admit).
    Nothing,
}

/// Checks a peer's proofs for a folder (or channel); admits a device that redeems an invitation when this device
/// is an owner or admin. `changed` is set when the membership changed (call [`changed`] once the session runs).
pub(crate) fn check(ctx: &Ctx, f: &Space, peer_pk: &[u8; 32], exporter: &[u8; 32], proofs: &[Proof], changed: &mut bool) -> Proved {
    let grants: Vec<Statement> = proofs
        .iter()
        .filter_map(|p| match p {
            Proof::Grant(b) => Statement::from_bytes(b).ok(),
            _ => None,
        })
        .collect();
    *changed |= add(f, grants);
    // An admin's device that asked the owners to remove this one keeps it out until their removal comes.
    if crate::removals::waiting(f, peer_pk) {
        return Proved::Refused(format!("an admin of this {} asked its owner to remove this device", noun(f)));
    }
    if let Some(r) = f.role_of(peer_pk) {
        return Proved::Member(r.access());
    }
    // One of the user's own devices, for a folder or channel that follows: admitted by an owner or admin here.
    if f.kind != SpaceKind::Devices
        && let Some(group) = proofs.iter().find_map(|p| match p {
            Proof::Sibling(b) => Some(b.as_slice()),
            _ => None,
        })
    {
        return crate::devices::check_sibling(ctx, f, peer_pk, group, changed);
    }
    let Some(invite) = proofs.iter().find_map(|p| match p {
        Proof::Invite(b) => check_redeem(b, peer_pk, exporter),
        _ => None,
    }) else {
        // An invitation of a later version: an owner or admin says it cannot read it, so the joining device knows.
        let newer = proofs.iter().any(|p| matches!(p, Proof::Invite(b) if b.first().is_some_and(|v| *v != INVITE_VERSION)));
        if newer && f.role_of(ctx.key.public_key()).is_some_and(Role::admits) {
            return Proved::Refused(format!("an owner or admin of this {} runs an older Seedwell that cannot read this link: it needs an update", noun(f)));
        }
        return Proved::Nothing;
    };
    let me = ctx.key.public_key();
    let now = now_s();
    let verdict = f.membership.read().unwrap_or_else(|e| e.into_inner()).check_admit(me, &invite, peer_pk, now);
    match verdict {
        Ok(Some(r)) => Proved::Member(r.access()),
        // A pairing link: only the device that made it admits, once the user there confirms.
        Ok(None) if f.kind == SpaceKind::Devices => crate::devices::pair_request(ctx, peer_pk, &invite),
        Ok(None) => {
            let grant = |serial| Body::Grant { device: *peer_pk, role: invite.role, serial, at_s: now, invite: Some(invite) };
            let added = match f.role_of(me) {
                // Checked again on the membership the grant joins (a use of the invitation counted meanwhile).
                Some(Role::Owner) => add_by_owner(f, |m, serials| match m.check_admit(me, &invite, peer_pk, now) {
                    Ok(None) => vec![grant(serials.next())],
                    _ => Vec::new(),
                }),
                _ => Some(add(f, [Statement::by_device(&ctx.key, &f.id, grant(0))])),
            };
            let Some(added) = added else { return Proved::Refused(format!("the {}'s owner key is missing on this device", noun(f))) };
            *changed |= added;
            match f.role_of(peer_pk) {
                Some(r) => Proved::Member(r.access()),
                None => Proved::Refused("the admission was not accepted".into()),
            }
        }
        // A device that is not an owner or admin does not admit: the joining device keeps looking.
        Err(trs_sync_proto::members::AdmitError::NotAllowed) if !f.role_of(me).is_some_and(Role::admits) => Proved::Nothing,
        Err(e) if f.kind == SpaceKind::Devices => Proved::Refused(crate::devices::refusal(e)),
        Err(e) => Proved::Refused(e.to_string()),
    }
}

/// A co-owner receives the folder (or channel) secret from an owner.
pub(crate) async fn owner_key(ctx: &Arc<Ctx>, f: &Arc<Space>, from: &[u8; 32], secret: [u8; 32]) {
    let me = *ctx.key.public_key();
    // A devices group this device moved on from keeps no key: its old key is worth nothing.
    if f.kind == SpaceKind::Devices && ctx.registry.devices().map(|g| g.id) != Some(f.id) {
        return;
    }
    let s = FolderSecret::from_bytes(secret);
    let fits = s.public_key() == f.public.public_key();
    if !fits || f.role_of(from) != Some(Role::Owner) || f.role_of(&me) != Some(Role::Owner) {
        return;
    }
    if f.secret.read().unwrap_or_else(|e| e.into_inner()).is_some() {
        return;
    }
    if let Err(e) = ctx.secrets.put(&secret_name(&f.id), s.as_bytes()) {
        f.problem(format!("the {}'s owner key could not be saved: {e}", noun(f)));
        return;
    }
    *f.secret.write().unwrap_or_else(|e| e.into_inner()) = Some(s);
    changed(ctx, f).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_membership_file_keeps_every_statement_and_says_its_format() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.members");
        let s = FolderSecret::from_bytes([4; 32]);
        let mut m = Membership::new(*s.public_key());
        let me = [7; 32];
        let later = Statement::by_owner(&s, Body::Unknown { kind: 200, data: vec![1, 2, 3] });
        m.add([Statement::by_owner(&s, Body::Grant { device: me, role: Role::Owner, serial: 1, at_s: 1, invite: None }), later.clone()]);
        save(&p, &m).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[..5], b"SWMB\x01");
        let back = load(&p, *s.public_key()).unwrap();
        assert!(back.statements().any(|st| *st == later), "a kind this version does not know survives");
        assert_eq!(back.role(&me), Some(Role::Owner));
        std::fs::write(&p, [&b"SWMB\x02"[..], &bytes[5..]].concat()).unwrap();
        assert!(load(&p, *s.public_key()).unwrap_err().to_string().contains("newer Seedwell"));
        std::fs::write(&p, b"SWM2").unwrap();
        assert!(load(&p, *s.public_key()).unwrap_err().to_string().contains("test build"));
    }

    /// Hooks [`add_by_owner`] runs once for a space, after `make` decided and before the statements are added.
    type Hook = Box<dyn FnOnce() + Send>;
    static DECIDED: std::sync::Mutex<std::collections::BTreeMap<String, Hook>> = std::sync::Mutex::new(std::collections::BTreeMap::new());

    pub(super) fn decided(id: &trs_sync_proto::ids::FolderId) {
        let hook = DECIDED.lock().unwrap_or_else(|e| e.into_inner()).remove(&id.to_string());
        if let Some(hook) = hook {
            hook();
        }
    }

    /// A folder whose owner is `owner` and admin `host`, with devices the host admitted at `admitted` (role, time).
    fn admitted_by_host(admitted: &[(Role, u64)]) -> (FolderSecret, Membership, Vec<trs_sync_proto::device::DeviceKey>) {
        use trs_sync_proto::device::DeviceKey;
        use trs_sync_proto::members::{Signer, new_invite};
        let s = FolderSecret::from_bytes([4; 32]);
        let fid = trs_sync_proto::ids::FolderId::of_public_key(s.public_key());
        let (owner, host) = (DeviceKey::from_seed(&[1; 32]), DeviceKey::from_seed(&[2; 32]));
        let mut m = Membership::new(*s.public_key());
        let by_owner = |device: &DeviceKey, role, serial| Statement::by_owner(&s, Body::Grant { device: *device.public_key(), role, serial, at_s: now_s(), invite: None });
        assert!(m.add([by_owner(&owner, Role::Owner, 1), by_owner(&host, Role::Admin, 2)]));
        let mut devices = Vec::new();
        for (n, (role, at)) in admitted.iter().enumerate() {
            let d = DeviceKey::from_seed(&[10 + n as u8; 32]);
            let sign = |b: &[u8]| host.sign(b);
            let i = new_invite(*s.public_key(), [n as u8; 16], [n as u8; 32], *role, 1, at + 3600, *at, Signer::Device(*host.public_key()), &sign);
            let grant = Body::Grant { device: *d.public_key(), role: *role, serial: 0, at_s: *at, invite: Some(i) };
            assert!(m.add([Statement::by_device(&host, &fid, grant)]));
            devices.push(d);
        }
        (s, m, devices)
    }

    fn member(m: &Membership, d: &trs_sync_proto::device::DeviceKey) -> trs_sync_proto::members::Member {
        *m.members().find(|(pk, _)| *pk == d.public_key()).unwrap().1
    }

    /// The owner signed an admin's admission again dated by its own clock: a device the host (an admin) admitted Read
    /// only under *Read only* (dated a second after it came on) and the owner confirmed in the second it came on counted
    /// as Read only from before, so turning it off left it Read only for good (a whole-suite run of
    /// `with_the_owner_away_an_admin_turning_read_only_on_holds_the_others_back_and_the_owner_lowers_the_right_ones`
    /// timed out on "S and N are admins"). Fails when confirming changes a member's time, or its role or invitation.
    #[test]
    fn an_owner_confirming_an_admins_admission_keeps_its_time() {
        // Admitted by the host: one ahead of the owner's clock (under Read only, the second after it came on), one
        // an hour before.
        let now = now_s();
        let admitted = [(Role::ReadOnly, now + 1), (Role::ReadWrite, now - 3600)];
        let (s, mut m, devices) = admitted_by_host(&admitted);
        let before: Vec<_> = devices.iter().map(|d| member(&m, d)).collect();
        assert!(before.iter().all(|mm| mm.by_admin));

        let mut serials = Serials(m.next_serial());
        let confirmed: Vec<Statement> = confirmations(&m, &mut serials).into_iter().map(|b| Statement::by_owner(&s, b)).collect();
        assert_eq!(confirmed.len(), 2);
        assert!(m.add(confirmed));
        for ((d, (role, at)), was) in devices.iter().zip(admitted).zip(before) {
            let after = member(&m, d);
            assert!(!after.by_admin, "confirmed by the owner");
            assert_eq!((after.role, after.invite, after.since_s), (role, was.invite, at), "confirming changed what the member is, or since when");
        }
        assert!(confirmations(&m, &mut Serials(0)).is_empty(), "once");
    }

    /// The owner's statements took their serial and were decided on one look at the membership, and added on another:
    /// a confirmation of a device an admin admitted Read only, decided just before the user made that device an admin
    /// and added just after, took the same serial (the signature bytes then decided) or a later one, and reset it to
    /// Read only for good. Forced here: the confirmation is held between its decision and its add while the user's
    /// choice is signed. Fails when two owner statements share a serial, or the device does not end up an admin.
    #[test]
    fn a_confirmation_and_a_new_role_at_once_leave_the_new_role_with_unique_serials() {
        use std::sync::mpsc::channel;
        use trs_sync_proto::ids::FolderId;
        use trs_sync_proto::keys::FolderKey;
        use trs_sync_proto::members::Signer;
        let came_on = now_s() - 10;
        let (s, m, devices) = admitted_by_host(&[(Role::ReadOnly, came_on + 1)]);
        let n = *devices[0].public_key();
        let dir = tempfile::tempdir().unwrap();
        let id = FolderId::of_public_key(s.public_key());
        let f = Arc::new(Space::new(id, SpaceKind::Folder, FolderKey::Member(*s.public_key()), Some(s), m, dir.path().join("f.members"), None, Default::default(), "Films".into()));

        // The confirmation decides, then waits.
        let (decided_tx, decided_rx) = channel();
        let (go_tx, go_rx) = channel::<()>();
        DECIDED.lock().unwrap().insert(id.to_string(), Box::new(move || {
            decided_tx.send(()).unwrap();
            let _ = go_rx.recv_timeout(std::time::Duration::from_secs(10));
        }));
        let confirm = {
            let f = f.clone();
            std::thread::spawn(move || add_by_owner(&f, confirmations))
        };
        decided_rx.recv_timeout(std::time::Duration::from_secs(10)).expect("the confirmation decided");
        // The user makes the device an admin meanwhile.
        let back = {
            let f = f.clone();
            std::thread::spawn(move || add_by_owner(&f, |_, serials| vec![Body::Grant { device: n, role: Role::Admin, serial: serials.next(), at_s: now_s(), invite: None }]))
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        go_tx.send(()).unwrap();
        assert_eq!(confirm.join().unwrap(), Some(true));
        back.join().unwrap();

        let m = f.membership.read().unwrap();
        assert_eq!(m.role(&n), Some(Role::Admin), "a confirmation decided earlier reset the device just made an admin");
        let serials: Vec<u64> = m
            .statements()
            .filter(|st| st.signer == Signer::Folder)
            .filter_map(|st| match st.body {
                Body::Grant { serial, .. } | Body::Remove { serial, .. } => Some(serial),
                _ => None,
            })
            .collect();
        let unique: std::collections::BTreeSet<u64> = serials.iter().copied().collect();
        assert_eq!(unique.len(), serials.len(), "two owner statements share a serial: {serials:?}");
    }

    /// Two owner devices of one folder (the user's devices sharing its ownership) switch *Read only* at once, in any
    /// order, each on what it knows, and their statements reach each other late and in any order. Fails when, once they
    /// have each other's statements, they read it differently, or when more rounds still change anything (nothing is
    /// said again by itself: only the user's switch says anything).
    #[test]
    fn two_owner_devices_switching_read_only_at_once_settle_on_one_answer() {
        let s = FolderSecret::from_bytes([4; 32]);
        let id = trs_sync_proto::ids::FolderId::of_public_key(s.public_key());
        let dir = tempfile::tempdir().unwrap();
        let mut seed: u64 = 0x5eed;
        let mut rand = move |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % n
        };
        for run in 0..300 {
            let owners: Vec<Arc<Space>> = (0..2)
                .map(|i| {
                    let public = trs_sync_proto::keys::FolderKey::Member(*s.public_key());
                    let path = dir.path().join(format!("{run}-{i}.members"));
                    Arc::new(Space::new(id, SpaceKind::Folder, public, Some(s.clone()), Membership::new(*s.public_key()), path, None, Default::default(), "Films".into()))
                })
                .collect();
            let deliver = |from: usize, to: usize| {
                let sts: Vec<Statement> = owners[from].membership.read().unwrap().statements().cloned().collect();
                owners[to].membership.write().unwrap().add(sts)
            };
            let mut said = Vec::new();
            for _ in 0..12 {
                match rand(3) {
                    0 | 1 => {
                        let (who, on) = (rand(2) as usize, rand(2) == 0);
                        let did = add_by_owner(&owners[who], |m, serials| read_only_statement(m, serials, on)) == Some(true);
                        said.push(format!("{who} {on} {did}"));
                    }
                    _ => {
                        let from = rand(2) as usize;
                        deliver(from, 1 - from);
                        said.push(format!("{from}->"));
                    }
                }
            }
            deliver(0, 1);
            deliver(1, 0);
            let a = owners[0].membership.read().unwrap().read_only();
            assert_eq!(a, owners[1].membership.read().unwrap().read_only(), "run {run}: {said:?}");
            for _ in 0..3 {
                assert!(!deliver(0, 1) && !deliver(1, 0), "run {run}: another round changed something");
            }
        }
    }
}
