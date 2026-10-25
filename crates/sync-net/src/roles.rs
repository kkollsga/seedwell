//! A folder's membership on this device (SPEC §7G.1): kept in `<id>.members` next to the index, proved when
//! connecting, applied when it changes.
//!
//! When the statements change, [`changed`] brings everything in line: the core's writer table and this device's
//! access, the sessions (a device that is no longer a member is cut off, the others get the new statements),
//! and an owner's duties (signing again what admins admitted, giving the folder secret to co-owners).

use std::io;
use std::path::Path;
use std::sync::Arc;

use trs_sync_proto::codec::{Reader, put_bytes, put_varint};
use trs_sync_proto::keys::{Access, FolderSecret, Proof};
use trs_sync_proto::members::{Body, INVITE_VERSION, Membership, Role, Statement, check_redeem, redeem_proof};
use trs_sync_proto::msg::{JoinFolder, Members, Message, OwnerKey};

use crate::shared::{Ctx, FolderShared, lock};

/// A membership file: `MAGIC ‖ format varint ‖ n × (varint length ‖ statement)`. Statements are kept as they
/// came (also kinds this version does not know), so passing them on later loses nothing.
const MAGIC: &[u8] = b"SWMB";
/// The layout of the membership file this version writes and reads.
const MEMBERS_FORMAT: u64 = 1;
/// What the test builds before 0.1.6 wrote.
const OLD_MAGIC: &[u8] = b"SWM2";

/// The secret store's name for a folder's secret `S` (owners only).
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
    let mut m = Membership::new(folder_pk);
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
    let mut out = MAGIC.to_vec();
    put_varint(&mut out, MEMBERS_FORMAT);
    for st in m.statements() {
        put_bytes(&mut out, &st.to_bytes());
    }
    let tmp = path.with_extension("members.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)
}

/// Adds statements to a folder's membership and keeps it on disk. True when roles or invitations changed (call
/// [`changed`] then).
pub(crate) fn add(f: &FolderShared, statements: impl IntoIterator<Item = Statement>) -> bool {
    let mut m = f.membership.write().unwrap_or_else(|e| e.into_inner());
    let ended = m.set_now(now_s());
    if !m.add(statements) {
        return ended;
    }
    if let Err(e) = save(&f.members_path, &m) {
        lock(&f.live).issues.push((String::new(), format!("the folder's membership could not be saved: {e}")));
    }
    true
}

/// The folder's sharing ended (or counts again) since the last look: true then (call [`changed`]).
pub(crate) fn tick(f: &FolderShared) -> bool {
    f.membership.write().unwrap_or_else(|e| e.into_inner()).set_now(now_s())
}

/// Signs `body` with the folder key, on an owner device.
pub(crate) fn by_owner(f: &FolderShared, body: Body) -> Option<Statement> {
    let secret = f.secret.read().unwrap_or_else(|e| e.into_inner()).clone()?;
    Some(Statement::by_owner(&secret, body))
}

/// The membership changed: bring the core, the sessions and this device's duties in line.
pub(crate) async fn changed(ctx: &Arc<Ctx>, f: &Arc<FolderShared>) {
    let me = *ctx.key.public_key();
    // An owner signs again what admins admitted, so those devices stay if that admin is later demoted.
    if f.role_of(&me) == Some(Role::Owner) {
        let todo = f.membership.read().unwrap_or_else(|e| e.into_inner()).to_confirm();
        let mut serial = f.membership.read().unwrap_or_else(|e| e.into_inner()).next_serial();
        let mut confirmed = Vec::new();
        for (device, role, invite) in todo {
            if let Some(st) = by_owner(f, Body::Grant { device, role, serial, at_s: now_s(), invite }) {
                confirmed.push(st);
                serial += 1;
            }
        }
        add(f, confirmed);
    }
    let (role, writers, statements) = {
        let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
        (m.role(&me), m.writers(), m.statements().cloned().collect::<Vec<_>>())
    };

    // The core: who writes, and whether this device does.
    let access = role.map_or(Access::Read, Role::access);
    let f2 = f.clone();
    let set = tokio::task::spawn_blocking(move || lock(&f2.core).set_membership(&writers, access).map_err(|e| e.to_string())).await;
    if let Ok(Err(e)) = set {
        lock(&f.live).issues.push((String::new(), format!("the folder's membership could not be applied: {e}")));
    }
    // Admitted: the invitation has done its job.
    if role.is_some() && lock(&f.invite).take().is_some() {
        let _ = ctx.secrets.delete(&invite_name(&f.id));
        lock(&f.live).refused = None;
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
                // Told first, so it knows; then the session ends.
                let _ = p.control.send(Message::Members(Members { folder_id: f.id, statements: statements.clone() })).await;
                let ended = f.membership.read().unwrap_or_else(|e| e.into_inner()).expired(&p.device_pk);
                let reason = if ended { "this folder is no longer shared" } else { "this device was removed from a folder" }.to_string();
                let _ = p.control.send(Message::Close(trs_sync_proto::msg::Close { reason })).await;
            }
            Some(r) => {
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
}

/// This device's proofs for a folder in a Join: its grant (and the admin's that signed it), or the invitation
/// it joins with, bound to the session. `None`: nothing to prove (removed, or never a member).
pub(crate) fn proofs(ctx: &Ctx, f: &FolderShared, exporter: &[u8; 32]) -> Option<Vec<Proof>> {
    let me = ctx.key.public_key();
    let grants = f.membership.read().unwrap_or_else(|e| e.into_inner()).proofs_for(me);
    if !grants.is_empty() {
        return Some(grants.iter().map(|st| Proof::Grant(st.to_bytes())).collect());
    }
    let invite = lock(&f.invite).clone()?;
    Some(vec![Proof::Invite(redeem_proof(&invite.invite, &invite.token_seed, me, exporter))])
}

/// What a peer's Join proves for one folder.
pub(crate) enum Proved {
    /// A member, with this access.
    Member(Access),
    /// It redeemed an invitation that this device refused, for this reason (told to the peer).
    Refused(String),
    /// Nothing this device can accept (a joining device met a member that does not admit).
    Nothing,
}

/// Checks a peer's proofs for a folder; admits a device that redeems an invitation when this device is an owner
/// or admin. `changed` is set when the membership changed (call [`changed`] once the session runs).
pub(crate) fn check(ctx: &Ctx, f: &FolderShared, peer_pk: &[u8; 32], exporter: &[u8; 32], jf: &JoinFolder, changed: &mut bool) -> Proved {
    let grants: Vec<Statement> = jf
        .proofs
        .iter()
        .filter_map(|p| match p {
            Proof::Grant(b) => Statement::from_bytes(b).ok(),
            _ => None,
        })
        .collect();
    *changed |= add(f, grants);
    if let Some(r) = f.role_of(peer_pk) {
        return Proved::Member(r.access());
    }
    let Some(invite) = jf.proofs.iter().find_map(|p| match p {
        Proof::Invite(b) => check_redeem(b, peer_pk, exporter),
        _ => None,
    }) else {
        // An invitation of a later version: an owner or admin says it cannot read it, so the joining device knows.
        let newer = jf.proofs.iter().any(|p| matches!(p, Proof::Invite(b) if b.first().is_some_and(|v| *v != INVITE_VERSION)));
        if newer && f.role_of(ctx.key.public_key()).is_some_and(Role::admits) {
            return Proved::Refused("an owner or admin of this folder runs an older Seedwell that cannot read this link: it needs an update".into());
        }
        return Proved::Nothing;
    };
    let me = ctx.key.public_key();
    let now = now_s();
    let verdict = f.membership.read().unwrap_or_else(|e| e.into_inner()).check_admit(me, &invite, peer_pk, now);
    match verdict {
        Ok(Some(r)) => Proved::Member(r.access()),
        Ok(None) => {
            let grant = Body::Grant { device: *peer_pk, role: invite.role, serial: 0, at_s: now, invite: Some(invite) };
            let st = match f.role_of(me) {
                Some(Role::Owner) => {
                    let serial = f.membership.read().unwrap_or_else(|e| e.into_inner()).next_serial();
                    let Body::Grant { device, role, at_s, invite, .. } = grant else { unreachable!() };
                    by_owner(f, Body::Grant { device, role, serial, at_s, invite })
                }
                _ => Some(Statement::by_device(&ctx.key, &f.id, grant)),
            };
            let Some(st) = st else { return Proved::Refused("the folder's owner key is missing on this device".into()) };
            *changed |= add(f, [st]);
            match f.role_of(peer_pk) {
                Some(r) => Proved::Member(r.access()),
                None => Proved::Refused("the admission was not accepted".into()),
            }
        }
        // A device that is not an owner or admin does not admit: the joining device keeps looking.
        Err(trs_sync_proto::members::AdmitError::NotAllowed) if !f.role_of(me).is_some_and(Role::admits) => Proved::Nothing,
        Err(e) => Proved::Refused(e.to_string()),
    }
}

/// A co-owner receives the folder secret from an owner.
pub(crate) async fn owner_key(ctx: &Arc<Ctx>, f: &Arc<FolderShared>, from: &[u8; 32], secret: [u8; 32]) {
    let me = *ctx.key.public_key();
    let s = FolderSecret::from_bytes(secret);
    let fits = s.public_key() == f.public.public_key();
    if !fits || f.role_of(from) != Some(Role::Owner) || f.role_of(&me) != Some(Role::Owner) {
        return;
    }
    if f.secret.read().unwrap_or_else(|e| e.into_inner()).is_some() {
        return;
    }
    if let Err(e) = ctx.secrets.put(&secret_name(&f.id), s.as_bytes()) {
        lock(&f.live).issues.push((String::new(), format!("the folder's owner key could not be saved: {e}")));
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
}
