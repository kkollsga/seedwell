//! Who is in a folder and what each device may do (SPEC §7G.1): roles, signed statements, invitations and
//! the links that carry them, and the rules every device applies to the statements it knows.
//!
//! A folder's **membership** is a set of signed statements. Every device keeps all it has seen, passes them on
//! and works out the same roles from them ([`Membership`]); there is no server. Owners sign with the folder key
//! (from `S`, which only they hold); admins sign with their device key, and only grants for devices they admit
//! through an invitation.
//!
//! ```text
//! statement = kind u8 ‖ body (varint length ‖ bytes) ‖ signer ‖ signature(64)
//!   signer  = 0 (the folder key) | 1 ‖ device public key(32)
//!   signed  = "seedwell-sync/v2/statement" ‖ folder_id ‖ kind ‖ body (with its length) ‖ signer
//!   1 grant   : device pk(32) ‖ role u8 ‖ serial varint ‖ at_s varint ‖ has_invite u8 [‖ invite (varint length ‖ bytes)]
//!   2 remove  : device pk(32) ‖ serial varint ‖ at_s varint
//!   3 invite  : invite                     (published, so every owner and admin knows it)
//!   4 revoke  : invite id(16) ‖ at_s varint
//!   5 share   : until_s varint (0 = no end) ‖ serial varint ‖ at_s varint     (owners only)
//! invite = version u8 (1) ‖ folder pk(32) ‖ id(16) ‖ role u8 ‖ max_uses varint (0 = unlimited)
//!          ‖ expires_s varint (0 = never) ‖ at_s varint ‖ issuer (signer) ‖ token pk(32) ‖ signature(64)
//!   signed  = "seedwell-sync/v2/invite" ‖ everything before the signature
//! link = seedwell://sync/<base32(invite without token pk ‖ token seed(32) ‖ check(2))>?name=<name>
//! redeem proof = invite ‖ token_sign("seedwell-sync/v2/redeem" ‖ folder_id ‖ device pk ‖ TLS exporter)
//! ```
//!
//! Evolution (`PROTOCOL.md`): a body may grow at its end (an older device keeps the bytes it does not know, so the
//! signature still checks and it passes the statement on unchanged); a kind it does not know, or a body it cannot
//! read (a role it does not know), is kept and passed on as [`Body::Unknown`] and gives no role. An invitation
//! never grows: a new form is a new invitation version, which older devices refuse ("made by a newer Seedwell").

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use ring::digest::{SHA256, digest};

use crate::base32;
use crate::codec::{DecodeError, Reader, put_bytes, put_varint};
use crate::device::{self, DeviceKey};
use crate::ids::FolderId;
use crate::keys::{Access, FolderSecret, KeyError, MAX_LINK_NAME, percent_decode, percent_encode};

const STATEMENT: &str = "seedwell-sync/v2/statement";
const INVITE: &str = "seedwell-sync/v2/invite";
const REDEEM: &str = "seedwell-sync/v2/redeem";
const LINK_CHECK: &str = "seedwell-sync/v2/link";
const LINK_PREFIX: &str = "seedwell://sync/";

/// Most uses a limited invitation may have; 0 means unlimited.
pub const MAX_INVITE_USES: u32 = 1000;
/// Statements one folder keeps (and one message may carry).
pub const MAX_STATEMENTS: usize = 10_000;
/// The invitation format this version writes and reads (the first byte of an invitation, and of a link).
pub const INVITE_VERSION: u8 = 1;

/// A device's role in a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// Receives and passes on changes, never sends its own.
    ReadOnly,
    /// Changes files.
    ReadWrite,
    /// Changes files and invites others (Read & write or Read only); cannot change roles.
    Admin,
    /// Holds the folder key: invites at any role, changes roles, removes devices.
    Owner,
}

impl Role {
    /// What the role may do with the files.
    pub fn access(self) -> Access {
        match self {
            Role::ReadOnly => Access::Read,
            _ => Access::Write,
        }
    }

    /// Owners and admins admit new devices.
    pub fn admits(self) -> bool {
        matches!(self, Role::Admin | Role::Owner)
    }

    fn byte(self) -> u8 {
        match self {
            Role::Owner => 1,
            Role::Admin => 2,
            Role::ReadWrite => 3,
            Role::ReadOnly => 4,
        }
    }

    fn from_byte(b: u8) -> Result<Role, DecodeError> {
        Ok(match b {
            1 => Role::Owner,
            2 => Role::Admin,
            3 => Role::ReadWrite,
            4 => Role::ReadOnly,
            _ => return Err(DecodeError::Invalid("role")),
        })
    }
}

/// Who signed a statement or an invitation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Signer {
    /// The folder key: an owner.
    Folder,
    /// A device key (an admin).
    Device([u8; 32]),
}

impl Signer {
    fn put(self, out: &mut Vec<u8>) {
        match self {
            Signer::Folder => out.push(0),
            Signer::Device(pk) => {
                out.push(1);
                out.extend_from_slice(&pk);
            }
        }
    }

    fn read(r: &mut Reader<'_>) -> Result<Signer, DecodeError> {
        match r.u8("signer")? {
            0 => Ok(Signer::Folder),
            1 => Ok(Signer::Device(r.array("signer key")?)),
            _ => Err(DecodeError::Invalid("signer")),
        }
    }

    fn key<'a>(&'a self, folder_pk: &'a [u8; 32]) -> &'a [u8; 32] {
        match self {
            Signer::Folder => folder_pk,
            Signer::Device(pk) => pk,
        }
    }
}

/// An invitation: who may join, at which role, until when and how often. Its token's secret half travels in the
/// link only; the token's public half is signed here.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Invite {
    /// The folder's public key.
    pub folder_pk: [u8; 32],
    /// Random; names the invitation in counts and revocations.
    pub id: [u8; 16],
    /// The role it admits at (an admin's invitations: Read & write or Read only).
    pub role: Role,
    /// Admissions it allows; 0 = unlimited.
    pub max_uses: u32,
    /// Unix seconds after which it admits nobody; 0 = never.
    pub expires_s: u64,
    /// When it was made (the issuer's clock).
    pub at_s: u64,
    /// The owner (folder key) or admin who made it.
    pub issuer: Signer,
    /// The token's public key.
    pub token_pk: [u8; 32],
    /// The issuer's signature.
    pub sig: [u8; 64],
}

impl fmt::Debug for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invite({}, {:?}, uses {}, expires {})", hex(&self.id), self.role, self.max_uses, self.expires_s)
    }
}

impl Invite {
    fn put_unsigned(&self, out: &mut Vec<u8>, with_token: bool) {
        out.push(INVITE_VERSION);
        out.extend_from_slice(&self.folder_pk);
        out.extend_from_slice(&self.id);
        out.push(self.role.byte());
        put_varint(out, u64::from(self.max_uses));
        put_varint(out, self.expires_s);
        put_varint(out, self.at_s);
        self.issuer.put(out);
        if with_token {
            out.extend_from_slice(&self.token_pk);
        }
    }

    fn signed_bytes(&self) -> Vec<u8> {
        let mut out = INVITE.as_bytes().to_vec();
        self.put_unsigned(&mut out, true);
        out
    }

    /// The wire form.
    pub fn put(&self, out: &mut Vec<u8>) {
        self.put_unsigned(out, true);
        out.extend_from_slice(&self.sig);
    }

    /// Reads the wire form (the signature is checked by [`Invite::verify`], not here).
    pub fn read(r: &mut Reader<'_>) -> Result<Invite, DecodeError> {
        Self::read_with(r, None)
    }

    fn read_with(r: &mut Reader<'_>, token_pk: Option<[u8; 32]>) -> Result<Invite, DecodeError> {
        if r.u8("invite version")? != INVITE_VERSION {
            return Err(DecodeError::Invalid("invite version"));
        }
        let folder_pk = r.array("folder key")?;
        let id = r.array("invite id")?;
        let role = Role::from_byte(r.u8("role")?)?;
        let max_uses = u32::try_from(r.varint("max uses")?).map_err(|_| DecodeError::Invalid("max uses"))?;
        let expires_s = r.varint("expiry")?;
        let at_s = r.varint("invite time")?;
        let issuer = Signer::read(r)?;
        let token_pk = match token_pk {
            Some(t) => t,
            None => r.array("token key")?,
        };
        Ok(Invite { folder_pk, id, role, max_uses, expires_s, at_s, issuer, token_pk, sig: r.array("invite signature")? })
    }

    /// The issuer's signature is good, and the role is one its issuer may hand out (an admin: below Admin).
    /// Whether the issuer is still an owner or admin is the membership's question.
    pub fn verify(&self) -> bool {
        let role_ok = match self.issuer {
            Signer::Folder => true,
            Signer::Device(_) => self.role <= Role::ReadWrite,
        };
        role_ok
            && self.max_uses <= MAX_INVITE_USES
            && device::verify(self.issuer.key(&self.folder_pk), &self.signed_bytes(), &self.sig)
    }

    /// The folder this invitation is for.
    pub fn folder_id(&self) -> FolderId {
        FolderId::of_public_key(&self.folder_pk)
    }

    /// Expired at `now_s`.
    pub fn expired(&self, now_s: u64) -> bool {
        self.expires_s != 0 && now_s > self.expires_s
    }
}

/// Makes a new invitation and its token. `sign` is the issuer's signing function (the folder key for an owner,
/// the device key for an admin). Returns the invitation and the token's seed (the link's secret part).
#[allow(clippy::too_many_arguments)]
pub fn new_invite(
    folder_pk: [u8; 32],
    id: [u8; 16],
    token_seed: [u8; 32],
    role: Role,
    max_uses: u32,
    expires_s: u64,
    at_s: u64,
    issuer: Signer,
    sign: &dyn Fn(&[u8]) -> [u8; 64],
) -> Invite {
    let token_pk = *DeviceKey::from_seed(&token_seed).public_key();
    let mut inv = Invite { folder_pk, id, role, max_uses, expires_s, at_s, issuer, token_pk, sig: [0; 64] };
    inv.sig = sign(&inv.signed_bytes());
    inv
}

/// What a statement says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// `device` has `role`. `invite`: the invitation it was admitted with (counts towards its uses).
    Grant {
        /// The device's public key.
        device: [u8; 32],
        /// Its role.
        role: Role,
        /// Orders an owner's statements about one device (higher wins).
        serial: u64,
        /// When it was made (the signer's clock).
        at_s: u64,
        /// The invitation it came from.
        invite: Option<Invite>,
    },
    /// `device` is out of the folder (owners only).
    Remove {
        /// The device's public key.
        device: [u8; 32],
        /// As for a grant.
        serial: u64,
        /// When it was made.
        at_s: u64,
    },
    /// An invitation, published so every owner and admin can show and count it.
    Invite(Invite),
    /// An invitation admits nobody from now on.
    Revoke {
        /// The invitation.
        id: [u8; 16],
        /// When it was revoked.
        at_s: u64,
    },
    /// How long the folder is shared (owners only): after `until_s` every member but the owners counts as
    /// removed and no invitation admits anyone; 0 = no end. An owner's latest (by serial) counts.
    ShareUntil {
        /// Unix seconds (each device's own clock); 0 = no end.
        until_s: u64,
        /// Orders owners' statements (higher wins).
        serial: u64,
        /// When it was made.
        at_s: u64,
    },
    /// A kind this version does not know, or a body it cannot read (a role it does not know): kept and passed
    /// on unchanged, never counted for roles.
    Unknown {
        /// The statement kind.
        kind: u8,
        /// The whole body.
        data: Vec<u8>,
    },
}

/// Longest statement body.
const MAX_BODY: usize = 1024;

/// A signed statement about a folder's membership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    /// What it says.
    pub body: Body,
    /// Who signed it.
    pub signer: Signer,
    /// The signature over [`Statement::signed_bytes`].
    pub sig: [u8; 64],
    /// Bytes a newer version appended to a body this version knows: kept, so the signature still checks and the
    /// statement is passed on as it came. Empty for statements this version makes.
    pub extra: Vec<u8>,
}

impl Statement {
    /// The kind byte and the body's bytes (`extra` at the end).
    fn body_bytes(body: &Body, extra: &[u8]) -> (u8, Vec<u8>) {
        let mut out = Vec::with_capacity(160);
        let kind = match body {
            Body::Grant { device, role, serial, at_s, invite } => {
                out.extend_from_slice(device);
                out.push(role.byte());
                put_varint(&mut out, *serial);
                put_varint(&mut out, *at_s);
                match invite {
                    Some(i) => {
                        out.push(1);
                        let mut b = Vec::with_capacity(200);
                        i.put(&mut b);
                        put_bytes(&mut out, &b);
                    }
                    None => out.push(0),
                }
                1
            }
            Body::Remove { device, serial, at_s } => {
                out.extend_from_slice(device);
                put_varint(&mut out, *serial);
                put_varint(&mut out, *at_s);
                2
            }
            Body::Invite(i) => {
                i.put(&mut out);
                3
            }
            Body::Revoke { id, at_s } => {
                out.extend_from_slice(id);
                put_varint(&mut out, *at_s);
                4
            }
            Body::ShareUntil { until_s, serial, at_s } => {
                put_varint(&mut out, *until_s);
                put_varint(&mut out, *serial);
                put_varint(&mut out, *at_s);
                5
            }
            Body::Unknown { kind, data } => {
                out.extend_from_slice(data);
                *kind
            }
        };
        out.extend_from_slice(extra);
        (kind, out)
    }

    fn put_unsigned(body: &Body, extra: &[u8], signer: Signer, out: &mut Vec<u8>) {
        let (kind, bytes) = Self::body_bytes(body, extra);
        out.push(kind);
        put_bytes(out, &bytes);
        signer.put(out);
    }

    fn signed_bytes(folder: &FolderId, body: &Body, extra: &[u8], signer: Signer) -> Vec<u8> {
        let mut out = STATEMENT.as_bytes().to_vec();
        out.extend_from_slice(&folder.0);
        Self::put_unsigned(body, extra, signer, &mut out);
        out
    }

    /// Signs `body` with the folder key (an owner).
    pub fn by_owner(secret: &FolderSecret, body: Body) -> Statement {
        let folder = FolderId::of_public_key(secret.public_key());
        let sig = secret.sign(&Self::signed_bytes(&folder, &body, &[], Signer::Folder));
        Statement { body, signer: Signer::Folder, sig, extra: Vec::new() }
    }

    /// Signs `body` with a device key (an admin).
    pub fn by_device(key: &DeviceKey, folder: &FolderId, body: Body) -> Statement {
        let signer = Signer::Device(*key.public_key());
        let sig = key.sign(&Self::signed_bytes(folder, &body, &[], signer));
        Statement { body, signer, sig, extra: Vec::new() }
    }

    /// The wire form.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(200);
        Self::put_unsigned(&self.body, &self.extra, self.signer, &mut out);
        out.extend_from_slice(&self.sig);
        out
    }

    /// Reads one statement (the signature is checked by [`Statement::verify`]). A kind this version does not know,
    /// or a body it cannot read, is [`Body::Unknown`]; bytes after a body it knows are kept in `extra`.
    pub fn read(r: &mut Reader<'_>) -> Result<Statement, DecodeError> {
        let kind = r.u8("statement kind")?;
        let data = r.bytes(MAX_BODY, "statement body")?;
        let (body, extra) = match Self::read_body(kind, data) {
            Some((body, rest)) => (body, rest.to_vec()),
            None => (Body::Unknown { kind, data: data.to_vec() }, Vec::new()),
        };
        Ok(Statement { body, signer: Signer::read(r)?, sig: r.array("statement signature")?, extra })
    }

    /// A body of a kind this version knows, and the bytes after it; `None` for any other.
    fn read_body(kind: u8, data: &[u8]) -> Option<(Body, &[u8])> {
        let r = &mut Reader::new(data);
        let body = match kind {
            1 => {
                let device = r.array("device key").ok()?;
                let role = Role::from_byte(r.u8("role").ok()?).ok()?;
                let serial = r.varint("serial").ok()?;
                let at_s = r.varint("time").ok()?;
                let invite = if r.bool("has invite").ok()? {
                    let b = r.bytes(MAX_BODY, "invite").ok()?;
                    Some(Invite::read(&mut Reader::new(b)).ok()?)
                } else {
                    None
                };
                Body::Grant { device, role, serial, at_s, invite }
            }
            2 => Body::Remove { device: r.array("device key").ok()?, serial: r.varint("serial").ok()?, at_s: r.varint("time").ok()? },
            3 => Body::Invite(Invite::read(r).ok()?),
            4 => Body::Revoke { id: r.array("invite id").ok()?, at_s: r.varint("time").ok()? },
            5 => Body::ShareUntil { until_s: r.varint("share end").ok()?, serial: r.varint("serial").ok()?, at_s: r.varint("time").ok()? },
            _ => return None,
        };
        let used = data.len() - r.remaining();
        Some((body, &data[used..]))
    }

    /// Reads a statement from its own bytes.
    pub fn from_bytes(b: &[u8]) -> Result<Statement, DecodeError> {
        Statement::read(&mut Reader::new(b))
    }

    /// The signature is good for `folder_pk`'s folder, and so is any invitation it carries. What the signer
    /// is allowed to say is the membership's question.
    pub fn verify(&self, folder_pk: &[u8; 32]) -> bool {
        let folder = FolderId::of_public_key(folder_pk);
        let invite_ok = |i: &Invite| i.folder_pk == *folder_pk && i.verify();
        let body_ok = match &self.body {
            Body::Grant { invite: Some(i), .. } => invite_ok(i),
            // A published invitation is signed by its issuer.
            Body::Invite(i) => invite_ok(i) && i.issuer == self.signer,
            // Only an owner removes or sets the share end; an admin's grant needs the invitation it admits with.
            Body::Remove { .. } | Body::ShareUntil { .. } => self.signer == Signer::Folder,
            Body::Grant { invite: None, .. } => self.signer == Signer::Folder,
            Body::Revoke { .. } => true,
            // Only the signature can be checked; it gives no role.
            Body::Unknown { .. } => true,
        };
        body_ok && device::verify(self.signer.key(folder_pk), &Self::signed_bytes(&folder, &self.body, &self.extra, self.signer), &self.sig)
    }

    fn hash(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(digest(&SHA256, &self.to_bytes()).as_ref());
        out
    }
}

/// What the session signs to redeem an invitation: binds the token, the joining device and the TLS session.
pub fn redeem_payload(folder: &FolderId, device_pk: &[u8; 32], exporter: &[u8; 32]) -> Vec<u8> {
    [REDEEM.as_bytes(), &folder.0, device_pk, exporter].concat()
}

/// A redeem proof's bytes: the invitation and the token's signature of the session.
pub fn redeem_proof(invite: &Invite, token_seed: &[u8; 32], device_pk: &[u8; 32], exporter: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    invite.put(&mut out);
    out.extend_from_slice(&DeviceKey::from_seed(token_seed).sign(&redeem_payload(&invite.folder_id(), device_pk, exporter)));
    out
}

/// Reads a redeem proof and checks the token's signature of this session for `device_pk`. `None`: not a good
/// proof (the invitation itself is still to be checked against the membership).
pub fn check_redeem(proof: &[u8], device_pk: &[u8; 32], exporter: &[u8; 32]) -> Option<Invite> {
    let r = &mut Reader::new(proof);
    let invite = Invite::read(r).ok()?;
    let sig: [u8; 64] = r.array("redeem signature").ok()?;
    let ok = invite.verify() && device::verify(&invite.token_pk, &redeem_payload(&invite.folder_id(), device_pk, exporter), &sig);
    ok.then_some(invite)
}

/// Why an invitation does not admit (a sentence for the person who used the link).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmitError {
    /// Not signed by an owner or a current admin of this folder.
    NotValid,
    /// Past its expiry.
    Expired,
    /// Every use is taken.
    UsedUp,
    /// Revoked by an owner or admin.
    Revoked,
    /// This device may not admit at that role (an admin, an Admin invitation).
    NotAllowed,
    /// The device was removed from the folder: only an owner can let it back in.
    Removed,
    /// The folder's sharing ended (its owner's setting): nobody joins until an owner extends it.
    NotShared,
}

impl fmt::Display for AdmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AdmitError::NotValid => "this link is not valid for this folder",
            AdmitError::Expired => "this link has expired",
            AdmitError::UsedUp => "this link has been used up",
            AdmitError::Revoked => "this link was revoked",
            AdmitError::NotAllowed => "only the folder's owner can admit with this link",
            AdmitError::Removed => "this device was removed from the folder; only the owner can add it again",
            AdmitError::NotShared => "this folder is no longer shared",
        })
    }
}

/// A member as the statements make it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    /// Its role.
    pub role: Role,
    /// The invitation it was admitted with, if any.
    pub invite: Option<[u8; 16]>,
    /// Its role rests on an admin's grant that no owner has signed again yet.
    pub by_admin: bool,
    /// The deciding statement's time.
    pub since_s: u64,
    decided_by: [u8; 32],
}

/// An invitation and how it stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InviteState {
    /// The invitation.
    pub invite: Invite,
    /// Devices admitted with it (and still in by it).
    pub uses: u32,
    /// Revoked.
    pub revoked: bool,
}

impl InviteState {
    /// Every use is taken.
    pub fn used_up(&self) -> bool {
        self.invite.max_uses != 0 && self.uses >= self.invite.max_uses
    }
}

/// A folder's membership: every statement this device knows, and the roles they give. Every device that knows
/// the same statements gets the same roles.
#[derive(Clone, Debug)]
pub struct Membership {
    folder_pk: [u8; 32],
    folder: FolderId,
    statements: BTreeMap<[u8; 32], Statement>,
    members: BTreeMap<[u8; 32], Member>,
    removed: BTreeSet<[u8; 32]>,
    invites: BTreeMap<[u8; 16], InviteState>,
    /// Invitations revoked by an owner or a current admin (also ones never published).
    revoked: BTreeSet<[u8; 16]>,
    max_serial: u64,
    /// When the folder stops being shared (the owners' latest `ShareUntil`); `None` = no end.
    share_until: Option<u64>,
    /// The statement that sets it (also when it lifted the end).
    share_by: Option<[u8; 32]>,
    /// The time [`Membership::set_now`] gave (Unix seconds): after the share end only the owners count.
    now_s: u64,
}

impl Membership {
    /// An empty membership for the folder with this public key.
    pub fn new(folder_pk: [u8; 32]) -> Self {
        Membership {
            folder_pk,
            folder: FolderId::of_public_key(&folder_pk),
            statements: BTreeMap::new(),
            members: BTreeMap::new(),
            removed: BTreeSet::new(),
            invites: BTreeMap::new(),
            revoked: BTreeSet::new(),
            max_serial: 0,
            share_until: None,
            share_by: None,
            now_s: 0,
        }
    }

    /// The folder.
    pub fn folder_id(&self) -> FolderId {
        self.folder
    }

    /// The folder's public key.
    pub fn folder_pk(&self) -> &[u8; 32] {
        &self.folder_pk
    }

    /// Adds statements (each checked; bad ones are skipped). True when the roles or invitations changed.
    ///
    /// An owner's statements always go in. A device's (an admin's grants, invitations and revocations) go in only
    /// while the signer is an owner or admin here, and only up to [`MAX_STATEMENTS`]: otherwise any member could fill
    /// the folder with statements until an owner's could not be kept any more. Roles only ever count a current
    /// admin's statements, so leaving the others out changes no role; a statement left out now comes again with the
    /// full set a peer sends when a session starts and after every change.
    pub fn add(&mut self, statements: impl IntoIterator<Item = Statement>) -> bool {
        let (mut by_owner, mut by_device): (Vec<Statement>, Vec<Statement>) =
            statements.into_iter().filter(|st| st.verify(&self.folder_pk)).partition(|st| st.signer == Signer::Folder);
        let before = (self.members.clone(), self.invites.clone(), self.share_until);
        let mut new = false;
        for st in by_owner.drain(..) {
            new |= self.statements.insert(st.hash(), st).is_none();
        }
        if new {
            self.resolve();
        }
        // Admins' statements may make another device an admin only through an owner, so one pass is enough.
        let mut more = false;
        for st in by_device.drain(..) {
            let Signer::Device(pk) = st.signer else { continue };
            if self.statements.len() >= MAX_STATEMENTS || !self.role(&pk).is_some_and(Role::admits) {
                continue;
            }
            more |= self.statements.insert(st.hash(), st).is_none();
        }
        if more {
            self.resolve();
        }
        (new || more) && before != (self.members.clone(), self.invites.clone(), self.share_until)
    }

    /// Tells the membership the time (this device's clock). True when the folder's sharing ended (or counts
    /// again, the clock having moved back): call the driver's change handling then.
    pub fn set_now(&mut self, now_s: u64) -> bool {
        let before: Vec<bool> = self.members.values().map(|m| self.active(m)).collect();
        self.now_s = now_s;
        before != self.members.values().map(|m| self.active(m)).collect::<Vec<_>>()
    }

    fn active(&self, m: &Member) -> bool {
        m.role == Role::Owner || !self.share_ended(self.now_s)
    }

    /// The folder's sharing has ended at `now_s`.
    pub fn share_ended(&self, now_s: u64) -> bool {
        self.share_until.is_some_and(|u| now_s > u)
    }

    /// When the folder stops being shared (the owners' setting); `None` = no end.
    pub fn share_until(&self) -> Option<u64> {
        self.share_until
    }

    /// The device is a member whose access ended with the folder's sharing.
    pub fn expired(&self, device_pk: &[u8; 32]) -> bool {
        self.members.get(device_pk).is_some_and(|m| !self.active(m))
    }

    /// Every statement, in a stable order (to send, or to store).
    pub fn statements(&self) -> impl Iterator<Item = &Statement> {
        self.statements.values()
    }

    /// The role of a device, if it is a member.
    pub fn role(&self, device_pk: &[u8; 32]) -> Option<Role> {
        self.members.get(device_pk).filter(|m| self.active(m)).map(|m| m.role)
    }

    /// The device was removed by an owner (and not let back in), or the folder's sharing ended for it.
    pub fn is_removed(&self, device_pk: &[u8; 32]) -> bool {
        self.removed.contains(device_pk) || self.expired(device_pk)
    }

    /// Every member that counts now (after the share end only the owners), by device key.
    pub fn members(&self) -> impl Iterator<Item = (&[u8; 32], &Member)> {
        self.members.iter().filter(|(_, m)| self.active(m))
    }

    /// The devices whose versions count: every member that counts now and may write.
    pub fn writers(&self) -> Vec<[u8; 32]> {
        self.members().filter(|(_, m)| m.role.access() == Access::Write).map(|(pk, _)| *pk).collect()
    }

    /// Every invitation known, by id.
    pub fn invites(&self) -> impl Iterator<Item = &InviteState> {
        self.invites.values()
    }

    /// One invitation.
    pub fn invite(&self, id: &[u8; 16]) -> Option<&InviteState> {
        self.invites.get(id)
    }

    /// The serial an owner's next statement gets (above every owner statement known).
    pub fn next_serial(&self) -> u64 {
        self.max_serial + 1
    }

    /// The statements that make `device_pk` a member: its deciding statement, for an admin's grant that admin's
    /// own grant, and the owners' share end if one was set. Sent as proofs when connecting, also once the share
    /// has ended (the other side decides), so an owner's later extension reaches a device that was cut off.
    pub fn proofs_for(&self, device_pk: &[u8; 32]) -> Vec<Statement> {
        let mut out = Vec::new();
        let Some(m) = self.members.get(device_pk) else { return out };
        if let Some(st) = self.statements.get(&m.decided_by) {
            if let Signer::Device(admin) = st.signer
                && let Some(a) = self.members.get(&admin)
                && let Some(ast) = self.statements.get(&a.decided_by)
            {
                out.push(ast.clone());
            }
            out.push(st.clone());
        }
        out.extend(self.share_by.and_then(|h| self.statements.get(&h)).cloned());
        out
    }

    /// Members whose role rests on an admin's grant only: an owner signs those again (same role), so they stay
    /// in even if that admin is later demoted.
    pub fn to_confirm(&self) -> Vec<([u8; 32], Role, Option<Invite>)> {
        self.members()
            .filter(|(_, m)| m.by_admin)
            .map(|(pk, m)| {
                let invite = self.statements.get(&m.decided_by).and_then(|st| match &st.body {
                    Body::Grant { invite, .. } => *invite,
                    _ => None,
                });
                (*pk, m.role, invite)
            })
            .collect()
    }

    /// May `admitter` (this device) admit `device_pk` with `invite` at `now_s`? `Ok(Some(role))`: the device is a
    /// member already; `Ok(None)`: sign a grant for it at `invite.role`.
    pub fn check_admit(&self, admitter: &[u8; 32], invite: &Invite, device_pk: &[u8; 32], now_s: u64) -> Result<Option<Role>, AdmitError> {
        if let Some(r) = self.role(device_pk) {
            return Ok(Some(r));
        }
        if self.share_ended(now_s) {
            return Err(AdmitError::NotShared);
        }
        let Some(mine) = self.role(admitter).filter(|r| r.admits()) else { return Err(AdmitError::NotAllowed) };
        if invite.folder_pk != self.folder_pk || !invite.verify() || !self.issuer_ok(&invite.issuer) {
            return Err(AdmitError::NotValid);
        }
        if mine == Role::Admin && invite.role > Role::ReadWrite {
            return Err(AdmitError::NotAllowed);
        }
        if self.is_removed(device_pk) && mine != Role::Owner {
            return Err(AdmitError::Removed);
        }
        if invite.expired(now_s) {
            return Err(AdmitError::Expired);
        }
        if self.revoked.contains(&invite.id) {
            return Err(AdmitError::Revoked);
        }
        if self.invites.get(&invite.id).is_some_and(InviteState::used_up) {
            return Err(AdmitError::UsedUp);
        }
        Ok(None)
    }

    /// An invitation's issuer is an owner, or an admin now.
    fn issuer_ok(&self, issuer: &Signer) -> bool {
        match issuer {
            Signer::Folder => true,
            Signer::Device(pk) => self.members.get(pk).is_some_and(|m| m.role.admits()),
        }
    }

    /// Works out every role from the statements. Owners decide first (their latest statement per device), then
    /// admins' grants for devices no owner has decided on; invitations used more often than allowed keep their
    /// earliest admissions (by time, then signature bytes) and the rest count as removed.
    fn resolve(&mut self) {
        let mut owner: BTreeMap<[u8; 32], (&Statement, [u8; 32])> = BTreeMap::new();
        let mut invites: BTreeMap<[u8; 16], InviteState> = BTreeMap::new();
        let mut revokes: Vec<(&Statement, [u8; 16])> = Vec::new();
        let mut share: Option<(u64, &[u8; 64], u64, [u8; 32])> = None;
        let mut max_serial = 0;
        for (h, st) in &self.statements {
            let note = |invites: &mut BTreeMap<[u8; 16], InviteState>, i: &Invite| {
                invites.entry(i.id).or_insert(InviteState { invite: *i, uses: 0, revoked: false });
            };
            if let Body::Grant { invite: Some(i), .. } | Body::Invite(i) = &st.body {
                note(&mut invites, i);
            }
            if let Body::Revoke { id, .. } = &st.body {
                revokes.push((st, *id));
            }
            if let (Body::ShareUntil { until_s, serial, .. }, Signer::Folder) = (&st.body, st.signer) {
                max_serial = max_serial.max(*serial);
                if share.is_none_or(|(cur, sig, _, _)| (*serial, &st.sig) > (cur, sig)) {
                    share = Some((*serial, &st.sig, *until_s, *h));
                }
            }
            let Some((device, serial)) = owner_target(st) else { continue };
            max_serial = max_serial.max(serial);
            let newer = owner.get(&device).is_none_or(|(cur, _)| (serial, &st.sig) > (statement_serial(cur), &cur.sig));
            if newer {
                owner.insert(device, (st, *h));
            }
        }

        // Owners' decisions; Admin (and Owner) roles come only from them.
        let mut members: BTreeMap<[u8; 32], Member> = BTreeMap::new();
        let mut removed = BTreeSet::new();
        let mut by_invite: Admissions = BTreeMap::new();
        for (device, (st, h)) in &owner {
            match &st.body {
                Body::Grant { role, at_s, invite, .. } => {
                    members.insert(*device, Member { role: *role, invite: invite.map(|i| i.id), by_admin: false, since_s: *at_s, decided_by: *h });
                    if let Some(i) = invite {
                        by_invite.entry(i.id).or_default().push(((*at_s, st.sig), *device));
                    }
                }
                _ => {
                    removed.insert(*device);
                }
            }
        }
        // Admin invitations are only an owner's, so their counts settle before admins act.
        limit(&mut members, &mut by_invite, &invites, &mut removed, |i| i.role >= Role::Admin);
        let admins: BTreeSet<[u8; 32]> = members.iter().filter(|(_, m)| m.role.admits()).map(|(pk, _)| *pk).collect();

        // Revocations by an owner or a current admin.
        let mut revoked = BTreeSet::new();
        for (st, id) in revokes {
            let by_ok = match st.signer {
                Signer::Folder => true,
                Signer::Device(pk) => admins.contains(&pk),
            };
            if by_ok {
                revoked.insert(id);
                if let Some(s) = invites.get_mut(&id) {
                    s.revoked = true;
                }
            }
        }
        let revoked_at = |id: &[u8; 16]| {
            self.statements
                .values()
                .filter_map(|st| match (&st.body, st.signer) {
                    (Body::Revoke { id: r, at_s }, s) if r == id && (s == Signer::Folder || matches!(s, Signer::Device(pk) if admins.contains(&pk))) => Some(*at_s),
                    _ => None,
                })
                .min()
        };

        // Admins' grants, for devices no owner has decided on: the earliest valid one.
        struct AdminGrant {
            key: (u64, [u8; 64]),
            role: Role,
            invite: Invite,
            hash: [u8; 32],
        }
        let mut admin_grants: BTreeMap<[u8; 32], AdminGrant> = BTreeMap::new();
        for (h, st) in &self.statements {
            let (Body::Grant { device, role, at_s, invite: Some(i), .. }, Signer::Device(admin)) = (&st.body, st.signer) else { continue };
            if owner.contains_key(device) || !admins.contains(&admin) || *role > i.role || i.role > Role::ReadWrite {
                continue;
            }
            let issuer_ok = match i.issuer {
                Signer::Folder => true,
                Signer::Device(pk) => admins.contains(&pk),
            };
            let in_time = !i.expired(*at_s) && revoked_at(&i.id).is_none_or(|r| *at_s < r);
            if !issuer_ok || !in_time {
                continue;
            }
            let key = (*at_s, st.sig);
            if admin_grants.get(device).is_none_or(|g| key < g.key) {
                admin_grants.insert(*device, AdminGrant { key, role: *role, invite: *i, hash: *h });
            }
        }
        for (device, g) in admin_grants {
            members.insert(device, Member { role: g.role, invite: Some(g.invite.id), by_admin: true, since_s: g.key.0, decided_by: g.hash });
            by_invite.entry(g.invite.id).or_default().push((g.key, device));
        }
        limit(&mut members, &mut by_invite, &invites, &mut removed, |i| i.role < Role::Admin);

        for (id, admitted) in &by_invite {
            if let Some(s) = invites.get_mut(id) {
                s.uses = admitted.iter().filter(|(_, d)| members.get(d).is_some_and(|m| m.invite == Some(*id))).count() as u32;
            }
        }
        self.members = members;
        self.removed = removed;
        self.invites = invites;
        self.revoked = revoked;
        self.max_serial = max_serial;
        self.share_until = share.map(|(_, _, u, _)| u).filter(|u| *u != 0);
        self.share_by = share.map(|(_, _, _, h)| h);
    }
}

/// The device and serial of an owner's grant or removal.
fn owner_target(st: &Statement) -> Option<([u8; 32], u64)> {
    match (&st.body, st.signer) {
        (Body::Grant { device, serial, .. } | Body::Remove { device, serial, .. }, Signer::Folder) => Some((*device, *serial)),
        _ => None,
    }
}

/// Admissions by invitation: the deciding statement's (time, signature) and the device.
type Admissions = BTreeMap<[u8; 16], Vec<((u64, [u8; 64]), [u8; 32])>>;

fn statement_serial(st: &Statement) -> u64 {
    match &st.body {
        Body::Grant { serial, .. } | Body::Remove { serial, .. } => *serial,
        _ => 0,
    }
}

/// Keeps the earliest admissions of each limited invitation that `applies` to; later ones lose their role
/// (unless an owner decided on them without that invitation).
fn limit(
    members: &mut BTreeMap<[u8; 32], Member>,
    by_invite: &mut Admissions,
    invites: &BTreeMap<[u8; 16], InviteState>,
    removed: &mut BTreeSet<[u8; 32]>,
    applies: impl Fn(&Invite) -> bool,
) {
    for (id, admitted) in by_invite.iter_mut() {
        let Some(st) = invites.get(id).filter(|s| applies(&s.invite)) else { continue };
        admitted.sort();
        if st.invite.max_uses == 0 {
            continue;
        }
        for (_, device) in admitted.iter().skip(st.invite.max_uses as usize) {
            if members.get(device).is_some_and(|m| m.invite == Some(*id)) {
                members.remove(device);
                removed.insert(*device);
            }
        }
    }
}

/// An invitation link: the invitation and its token's secret. The link is the secret: show it to the person,
/// never log it.
#[derive(Clone, PartialEq, Eq)]
pub struct InviteLink {
    /// The invitation.
    pub invite: Invite,
    /// The token's seed.
    pub token_seed: [u8; 32],
    /// The folder's name, a hint for the joining device.
    pub name: Option<String>,
}

impl fmt::Debug for InviteLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InviteLink").field("invite", &self.invite).field("name", &self.name).finish()
    }
}

impl InviteLink {
    /// The link's text.
    pub fn format(&self) -> String {
        let mut bytes = Vec::with_capacity(200);
        self.invite.put_unsigned(&mut bytes, false);
        bytes.extend_from_slice(&self.invite.sig);
        bytes.extend_from_slice(&self.token_seed);
        let check = link_check(&bytes);
        bytes.extend_from_slice(&check);
        let mut out = format!("{LINK_PREFIX}{}", base32::encode(&bytes));
        if let Some(name) = self.name.as_deref().filter(|n| !n.is_empty()) {
            out.push_str("?name=");
            out.push_str(&percent_encode(name));
        }
        out
    }

    /// Parses a link. Surrounding whitespace and the `<` `>` quotes mail clients put around links are allowed;
    /// query fields other than `name` are ignored (later versions may add some). A bare key of the earlier test
    /// builds is refused with [`KeyError::OldKey`].
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        let mut text = text.trim();
        if let Some(inner) = text.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
            text = inner.trim();
        }
        if crate::keys::is_old_key(text) {
            return Err(KeyError::OldKey);
        }
        let has_scheme = text.len() >= LINK_PREFIX.len()
            && text.is_char_boundary(LINK_PREFIX.len())
            && text[..LINK_PREFIX.len()].eq_ignore_ascii_case(LINK_PREFIX);
        if !has_scheme {
            return Err(KeyError::Link);
        }
        let rest = &text[LINK_PREFIX.len()..];
        let (body, query) = rest.split_once('?').unwrap_or((rest, ""));
        if crate::keys::is_old_key(body) {
            return Err(KeyError::OldKey);
        }
        let bytes = base32::decode(body).map_err(|_| KeyError::Damaged)?;
        if bytes.len() < 2 + 32 {
            return Err(KeyError::Damaged);
        }
        let (payload, check) = bytes.split_at(bytes.len() - 2);
        if link_check(payload) != check {
            return Err(KeyError::Damaged);
        }
        let (inv_bytes, seed) = payload.split_at(payload.len() - 32);
        // A link of a later invitation version: only a newer Seedwell reads it.
        if inv_bytes.first().is_some_and(|v| *v != INVITE_VERSION) {
            return Err(KeyError::Newer);
        }
        let token_seed: [u8; 32] = seed.try_into().map_err(|_| KeyError::Damaged)?;
        let token_pk = *DeviceKey::from_seed(&token_seed).public_key();
        let r = &mut Reader::new(inv_bytes);
        let invite = Invite::read_with(r, Some(token_pk)).map_err(|_| KeyError::Damaged)?;
        if r.remaining() != 0 || !invite.verify() {
            return Err(KeyError::Damaged);
        }
        let mut name = None;
        for field in query.split('&') {
            if let Some(v) = field.strip_prefix("name=") {
                name = Some(percent_decode(v)?).filter(|n| !n.is_empty() && n.len() <= MAX_LINK_NAME);
            }
        }
        Ok(InviteLink { invite, token_seed, name })
    }
}

fn link_check(payload: &[u8]) -> [u8; 2] {
    let d = digest(&SHA256, &[LINK_CHECK.as_bytes(), payload].concat());
    [d.as_ref()[0], d.as_ref()[1]]
}

/// Lowercase hex (invite ids in text form).
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// An invite id from its hex form.
pub fn invite_id_from_hex(s: &str) -> Option<[u8; 16]> {
    let s = s.trim();
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn owner() -> FolderSecret {
        FolderSecret::from_bytes([1; 32])
    }

    fn dev(n: u8) -> DeviceKey {
        DeviceKey::from_seed(&[n; 32])
    }

    fn grant(s: &FolderSecret, d: &DeviceKey, role: Role, serial: u64) -> Statement {
        Statement::by_owner(s, Body::Grant { device: *d.public_key(), role, serial, at_s: NOW, invite: None })
    }

    fn invite_by(s: &FolderSecret, issuer: Option<&DeviceKey>, role: Role, uses: u32, n: u8) -> Invite {
        let who = issuer.map_or(Signer::Folder, |k| Signer::Device(*k.public_key()));
        let sign = |m: &[u8]| match issuer {
            None => s.sign(m),
            Some(k) => k.sign(m),
        };
        new_invite(*s.public_key(), [n; 16], [n; 32], role, uses, NOW + 86_400, NOW, who, &sign)
    }

    fn admitted_by_admin(s: &FolderSecret, admin: &DeviceKey, d: &DeviceKey, i: Invite, at: u64) -> Statement {
        Statement::by_device(admin, &FolderId::of_public_key(s.public_key()), Body::Grant { device: *d.public_key(), role: i.role, serial: 0, at_s: at, invite: Some(i) })
    }

    fn base() -> (FolderSecret, Membership, DeviceKey) {
        let s = owner();
        let me = dev(10);
        let mut m = Membership::new(*s.public_key());
        assert!(m.add([grant(&s, &me, Role::Owner, 1)]));
        (s, m, me)
    }

    #[test]
    fn statements_roundtrip_and_verify() {
        let s = owner();
        let a = dev(2);
        let i = invite_by(&s, None, Role::ReadWrite, 3, 7);
        let sts = [
            grant(&s, &a, Role::Admin, 4),
            Statement::by_owner(&s, Body::Remove { device: *a.public_key(), serial: 5, at_s: NOW }),
            Statement::by_owner(&s, Body::Invite(i)),
            Statement::by_owner(&s, Body::Revoke { id: i.id, at_s: NOW }),
            admitted_by_admin(&s, &a, &dev(3), i, NOW),
        ];
        for st in sts {
            let back = Statement::from_bytes(&st.to_bytes()).unwrap();
            assert_eq!(back, st);
            assert!(back.verify(s.public_key()));
            assert!(!back.verify(FolderSecret::from_bytes([2; 32]).public_key()), "another folder");
            let mut bad = back.clone();
            bad.sig[3] ^= 1;
            assert!(!bad.verify(s.public_key()));
        }
    }

    #[test]
    fn only_owners_remove_or_grant_without_an_invitation() {
        let s = owner();
        let fid = FolderId::of_public_key(s.public_key());
        let admin = dev(2);
        let no_invite = Statement::by_device(&admin, &fid, Body::Grant { device: [9; 32], role: Role::Owner, serial: 9, at_s: NOW, invite: None });
        let remove = Statement::by_device(&admin, &fid, Body::Remove { device: [9; 32], serial: 9, at_s: NOW });
        assert!(!no_invite.verify(s.public_key()));
        assert!(!remove.verify(s.public_key()));
    }

    #[test]
    fn the_latest_owner_statement_decides_and_every_order_gives_the_same_roles() {
        let (s, _, me) = base();
        let a = dev(2);
        let sts = vec![
            grant(&s, &me, Role::Owner, 1),
            grant(&s, &a, Role::ReadWrite, 2),
            grant(&s, &a, Role::Admin, 3),
            Statement::by_owner(&s, Body::Remove { device: *a.public_key(), serial: 4, at_s: NOW }),
            grant(&s, &a, Role::ReadOnly, 5),
        ];
        let mut fwd = Membership::new(*s.public_key());
        fwd.add(sts.clone());
        let mut rev = Membership::new(*s.public_key());
        for st in sts.into_iter().rev() {
            rev.add([st]);
        }
        assert_eq!(fwd.role(a.public_key()), Some(Role::ReadOnly));
        assert_eq!(rev.role(a.public_key()), Some(Role::ReadOnly));
        assert_eq!(fwd.next_serial(), 6);
        assert_eq!(fwd.writers(), vec![*me.public_key()]);
    }

    #[test]
    fn a_removed_device_is_out_and_admins_cannot_bring_it_back() {
        let (s, mut m, me) = base();
        let (admin, x) = (dev(2), dev(3));
        m.add([grant(&s, &admin, Role::Admin, 2), grant(&s, &x, Role::ReadWrite, 3)]);
        m.add([Statement::by_owner(&s, Body::Remove { device: *x.public_key(), serial: 4, at_s: NOW })]);
        assert_eq!(m.role(x.public_key()), None);
        assert!(m.is_removed(x.public_key()));
        assert!(!m.writers().contains(x.public_key()), "its later versions are not accepted");
        let i = invite_by(&s, Some(&admin), Role::ReadWrite, 0, 5);
        assert_eq!(m.check_admit(admin.public_key(), &i, x.public_key(), NOW), Err(AdmitError::Removed));
        m.add([admitted_by_admin(&s, &admin, &x, i, NOW)]);
        assert_eq!(m.role(x.public_key()), None, "an owner's removal outranks an admin's grant");
        // The owner can.
        assert_eq!(m.check_admit(me.public_key(), &i, x.public_key(), NOW), Ok(None));
    }

    #[test]
    fn admins_invite_below_admin_and_cannot_change_roles() {
        let (s, mut m, _me) = base();
        let (admin, member, newcomer) = (dev(2), dev(3), dev(4));
        m.add([grant(&s, &admin, Role::Admin, 2), grant(&s, &member, Role::ReadOnly, 3)]);
        let fid = m.folder_id();
        // A role change by an admin is not even a valid statement (no invitation); with an invitation, an owner's
        // decision about that device wins.
        let i = invite_by(&s, Some(&admin), Role::ReadWrite, 0, 6);
        m.add([admitted_by_admin(&s, &admin, &member, i, NOW)]);
        assert_eq!(m.role(member.public_key()), Some(Role::ReadOnly));
        // Admins' invitations cannot be at Admin.
        let mut up = invite_by(&s, Some(&admin), Role::ReadWrite, 0, 7);
        up.role = Role::Admin;
        up.sig = admin.sign(&up.signed_bytes());
        assert!(!up.verify());
        let promote = Statement::by_device(&admin, &fid, Body::Grant { device: *newcomer.public_key(), role: Role::Admin, serial: 0, at_s: NOW, invite: Some(up) });
        m.add([promote]);
        assert_eq!(m.role(newcomer.public_key()), None);
        // An ordinary admission works.
        assert_eq!(m.check_admit(admin.public_key(), &i, newcomer.public_key(), NOW), Ok(None));
        m.add([admitted_by_admin(&s, &admin, &newcomer, i, NOW)]);
        assert_eq!(m.role(newcomer.public_key()), Some(Role::ReadWrite));
        assert_eq!(m.to_confirm().len(), 1, "an owner signs it again");
        let proofs = m.proofs_for(newcomer.public_key());
        assert_eq!(proofs.len(), 2, "its grant and the admin's");
        let mut fresh = Membership::new(*s.public_key());
        fresh.add(proofs);
        assert_eq!(fresh.role(newcomer.public_key()), Some(Role::ReadWrite), "the proofs alone make it a member");
    }

    #[test]
    fn a_demoted_admins_unconfirmed_admissions_lapse_and_confirmed_ones_stay() {
        let (s, mut m, me) = base();
        let (admin, a, b) = (dev(2), dev(3), dev(4));
        m.add([grant(&s, &admin, Role::Admin, 2)]);
        let i = invite_by(&s, None, Role::ReadWrite, 0, 8);
        m.add([admitted_by_admin(&s, &admin, &a, i, NOW), admitted_by_admin(&s, &admin, &b, i, NOW + 1)]);
        let _ = me;
        // The owner confirms a.
        m.add([Statement::by_owner(&s, Body::Grant { device: *a.public_key(), role: Role::ReadWrite, serial: 3, at_s: NOW + 2, invite: Some(i) })]);
        m.add([grant(&s, &admin, Role::ReadWrite, 4)]);
        assert_eq!(m.role(a.public_key()), Some(Role::ReadWrite));
        assert_eq!(m.role(b.public_key()), None);
        assert_eq!(m.invite(&i.id).unwrap().uses, 1);
    }

    #[test]
    fn limits_expiry_and_revocation_hold() {
        let (s, mut m, me) = base();
        let i = invite_by(&s, None, Role::ReadOnly, 1, 9);
        m.add([Statement::by_owner(&s, Body::Invite(i))]);
        let (a, b) = (dev(3), dev(4));
        assert_eq!(m.check_admit(me.public_key(), &i, a.public_key(), NOW), Ok(None));
        m.add([Statement::by_owner(&s, Body::Grant { device: *a.public_key(), role: Role::ReadOnly, serial: 2, at_s: NOW, invite: Some(i) })]);
        let st = m.invite(&i.id).unwrap();
        assert_eq!((st.uses, st.used_up()), (1, true));
        assert_eq!(m.check_admit(me.public_key(), &i, b.public_key(), NOW), Err(AdmitError::UsedUp));
        assert_eq!(m.check_admit(me.public_key(), &i, a.public_key(), NOW), Ok(Some(Role::ReadOnly)), "a member already");
        let j = invite_by(&s, None, Role::ReadOnly, 0, 10);
        assert_eq!(m.check_admit(me.public_key(), &j, b.public_key(), NOW + 2 * 86_400), Err(AdmitError::Expired));
        m.add([Statement::by_owner(&s, Body::Revoke { id: j.id, at_s: NOW })]);
        assert_eq!(m.check_admit(me.public_key(), &j, b.public_key(), NOW), Err(AdmitError::Revoked));
        let ro = dev(5);
        m.add([grant(&s, &ro, Role::ReadOnly, 9)]);
        assert_eq!(m.check_admit(ro.public_key(), &j, b.public_key(), NOW), Err(AdmitError::NotAllowed), "a reader does not admit");
    }

    #[test]
    fn two_admins_over_the_limit_keep_the_same_earliest_device_everywhere() {
        let (s, mut base_m, _me) = base();
        let (a1, a2, x, y) = (dev(2), dev(3), dev(4), dev(5));
        base_m.add([grant(&s, &a1, Role::Admin, 2), grant(&s, &a2, Role::Admin, 3)]);
        let i = invite_by(&s, None, Role::ReadWrite, 1, 11);
        let gx = admitted_by_admin(&s, &a1, &x, i, NOW + 5);
        let gy = admitted_by_admin(&s, &a2, &y, i, NOW + 3);
        for order in [vec![gx.clone(), gy.clone()], vec![gy.clone(), gx.clone()]] {
            let mut m = base_m.clone();
            for g in order {
                m.add([g]);
            }
            assert_eq!(m.role(y.public_key()), Some(Role::ReadWrite), "the earlier admission stays");
            assert_eq!(m.role(x.public_key()), None);
            assert_eq!(m.invite(&i.id).unwrap().uses, 1);
        }
    }

    #[test]
    fn the_share_end_removes_everyone_but_the_owners_until_an_owner_extends_it() {
        let (s, mut m, me) = base();
        let (admin, guest) = (dev(2), dev(3));
        m.add([grant(&s, &admin, Role::Admin, 2), grant(&s, &guest, Role::ReadWrite, 3)]);
        let until = |u: u64, serial: u64| Statement::by_owner(&s, Body::ShareUntil { until_s: u, serial, at_s: NOW });
        assert!(m.add([until(NOW + 7 * 86_400, 4)]), "a new share end is a change");
        assert_eq!((m.share_until(), m.next_serial()), (Some(NOW + 7 * 86_400), 5));
        assert!(!m.set_now(NOW + 86_400), "still shared");
        assert_eq!(m.role(guest.public_key()), Some(Role::ReadWrite));
        let i = invite_by(&s, None, Role::ReadOnly, 0, 16);
        assert_eq!(m.check_admit(me.public_key(), &i, dev(4).public_key(), NOW + 86_400), Ok(None));

        // The end passes: only the owner counts, and nobody joins.
        assert!(m.set_now(NOW + 8 * 86_400));
        assert_eq!((m.role(guest.public_key()), m.role(admin.public_key())), (None, None));
        assert_eq!(m.role(me.public_key()), Some(Role::Owner));
        assert!(m.is_removed(guest.public_key()) && m.expired(admin.public_key()));
        assert_eq!(m.writers(), vec![*me.public_key()]);
        assert_eq!(m.members().count(), 1);
        assert_eq!(m.proofs_for(guest.public_key()), vec![grant(&s, &guest, Role::ReadWrite, 3), until(NOW + 7 * 86_400, 4)], "the share end goes along");
        assert_eq!(m.check_admit(me.public_key(), &i, dev(4).public_key(), NOW + 8 * 86_400), Err(AdmitError::NotShared));
        assert_eq!(m.check_admit(me.public_key(), &i, guest.public_key(), NOW + 8 * 86_400), Err(AdmitError::NotShared));

        // An admin cannot set it; an owner extends it, or lifts it, and they count again.
        let fid = m.folder_id();
        let by_admin = Statement::by_device(&admin, &fid, Body::ShareUntil { until_s: 0, serial: 99, at_s: NOW });
        assert!(!by_admin.verify(s.public_key()));
        m.add([by_admin]);
        assert_eq!(m.role(guest.public_key()), None);
        m.add([until(NOW + 30 * 86_400, 5)]);
        assert_eq!(m.role(guest.public_key()), Some(Role::ReadWrite));
        m.add([until(0, 6)]);
        assert_eq!((m.share_until(), m.role(admin.public_key())), (None, Some(Role::Admin)));
        assert!(!m.set_now(NOW + 400 * 86_400), "no end any more");
        // The latest by serial wins, whatever order they arrive in.
        let mut other = Membership::new(*s.public_key());
        other.add([until(0, 6), until(NOW, 4), grant(&s, &me, Role::Owner, 1)]);
        assert_eq!(other.share_until(), None);
        let stmt = until(NOW, 4);
        assert_eq!(Statement::from_bytes(&stmt.to_bytes()).unwrap(), stmt);
    }

    #[test]
    fn only_owners_and_admins_can_add_statements_so_nobody_can_crowd_out_the_owner() {
        // `me` in `base` is an owner device: it signs as a device, like an admin.
        let (s, mut m, admin) = base();
        let reader = dev(40);
        m.add([grant(&s, &reader, Role::ReadOnly, 50)]);
        let before = m.statements().count();
        // A read-only member's (well signed) revocations are not kept at all.
        let junk: Vec<Statement> = (0..50u64)
            .map(|n| Statement::by_device(&reader, &m.folder_id(), Body::Revoke { id: [n as u8; 16], at_s: n }))
            .collect();
        assert!(!m.add(junk));
        assert_eq!(m.statements().count(), before, "nothing from a non-admin is stored");
        // An admin's revocation is.
        let i = invite_by(&s, Some(&admin), Role::ReadWrite, 1, 7);
        m.add([Statement::by_device(&admin, &m.folder_id(), Body::Revoke { id: i.id, at_s: 5 })]);
        assert_eq!(m.statements().count(), before + 1);
        // And an owner's statement goes in even when the folder is full.
        let mut full = m.clone();
        let filler: Vec<Statement> = (0..MAX_STATEMENTS as u64)
            .map(|n| Statement::by_device(&admin, &full.folder_id(), Body::Revoke { id: { let mut b = [0u8; 16]; b[..8].copy_from_slice(&n.to_be_bytes()); b }, at_s: n }))
            .collect();
        full.add(filler);
        assert!(full.statements().count() <= MAX_STATEMENTS);
        assert!(full.add([Statement::by_owner(&s, Body::Remove { device: *reader.public_key(), serial: 60, at_s: 60 })]));
        assert!(full.is_removed(reader.public_key()));
    }

    #[test]
    fn links_roundtrip_and_tolerate_mail() {
        let s = owner();
        let i = invite_by(&s, None, Role::ReadWrite, 1, 12);
        let link = InviteLink { invite: i, token_seed: [12; 32], name: Some("Bilder & ting/2026 ✓".into()) };
        let text = link.format();
        assert!(text.starts_with("seedwell://sync/"), "{text}");
        assert!(!text[16..].contains(' ') && !text[16..].contains('&') && !text[16..].contains('/'));
        assert_eq!(InviteLink::parse(&text).unwrap(), link);
        assert_eq!(InviteLink::parse(&format!("  <{text}>\n")).unwrap(), link);
        assert_eq!(InviteLink::parse(&text.to_lowercase().replace("seedwell://sync/", "SEEDWELL://SYNC/")).map(|l| l.invite), Ok(i), "either case");
        assert!(text.len() < 340, "{} characters", text.len());
    }

    #[test]
    fn bad_links_and_old_keys_are_refused() {
        let s = owner();
        let text = InviteLink { invite: invite_by(&s, None, Role::ReadOnly, 0, 13), token_seed: [13; 32], name: None }.format();
        assert_eq!(InviteLink::parse("SWW-ABCDEF"), Err(KeyError::OldKey));
        assert_eq!(InviteLink::parse("seedwell://sync/SWR-ABCDEF?name=x"), Err(KeyError::OldKey));
        assert_eq!(InviteLink::parse("https://example.com"), Err(KeyError::Link));
        assert_eq!(InviteLink::parse(&text[..text.len() - 3]), Err(KeyError::Damaged));
        let mut typo = text.clone().into_bytes();
        let k = typo.len() / 2;
        typo[k] = if typo[k] == b'A' { b'B' } else { b'A' };
        assert_eq!(InviteLink::parse(&String::from_utf8(typo).unwrap()), Err(KeyError::Damaged));
        assert_eq!(InviteLink::parse(&format!("{text}?name=%G1")), Err(KeyError::Name));
    }

    #[test]
    fn a_redeem_proof_is_bound_to_the_device_and_the_session() {
        let s = owner();
        let i = invite_by(&s, None, Role::ReadOnly, 1, 14);
        let d = dev(3);
        let proof = redeem_proof(&i, &[14; 32], d.public_key(), &[7; 32]);
        assert_eq!(check_redeem(&proof, d.public_key(), &[7; 32]), Some(i));
        assert_eq!(check_redeem(&proof, dev(4).public_key(), &[7; 32]), None, "another device");
        assert_eq!(check_redeem(&proof, d.public_key(), &[8; 32]), None, "another session");
        let wrong_token = redeem_proof(&i, &[15; 32], d.public_key(), &[7; 32]);
        assert_eq!(check_redeem(&wrong_token, d.public_key(), &[7; 32]), None, "a token the invitation does not name");
    }
}
