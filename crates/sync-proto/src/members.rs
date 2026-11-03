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
//!   6 profile : device name str ‖ user name str (empty = none) ‖ serial varint ‖ at_s varint
//!               (signed by the device it names, a member)
//!   7 channel : flags varint ‖ at_s varint     (owners only; a channel's own facts, 0.1.12)
//!               flags bit 0: open to agents (0.1.12 read it as set for good; since 0.1.13 an Agents: Yes at serial 0)
//!   8 agents  : open u8 (0 = No, 1 = Yes) ‖ serial varint ‖ at_s varint     (owners and admins, 0.1.13)
//!               a channel's "Agents: Yes/No": the owners' and current admins' latest (by serial) counts
//!   9 follow  : target u8 (1 folder, 2 channel) ‖ target pk(32) ‖ name str ‖ on u8 ‖ serial varint ‖ at_s varint
//!               (0.1.13, only in a devices group, signed by the device that says it, a member): "on all my devices";
//!               per target the latest (by serial, then time, then signature) of a current member counts
//!  10 moved   : target pk(32) ‖ at_s varint     (0.1.13, only in a devices group, signed by a member's device key):
//!               the group moved to the group with this key (its key changed after a removal); of a current
//!               member's, the lowest target group id counts
//!  11 rendezvous : url str (≤ 200 bytes; empty: none) ‖ serial varint ‖ at_s varint     (0.1.13, only in a devices group,
//!               signed by the device it is about): the rendezvous this device uses, so the others without one take
//!               it; its latest (by serial) counts
//! invite = version u8 (1) ‖ folder pk(32) ‖ id(16) ‖ role u8 ‖ max_uses varint (0 = unlimited)
//!          ‖ expires_s varint (0 = never) ‖ at_s varint ‖ issuer (signer) ‖ token pk(32) ‖ signature(64)
//!   signed  = "seedwell-sync/v2/invite" ‖ everything before the signature
//! link = seedwell://sync/<base32(invite without token pk ‖ token seed(32) ‖ check(2))>?name=<name>
//!        (a channel's: seedwell://chat/<the same>?name=<name>[&agents=yes]; a folder's with its channel, one link
//!        for both: seedwell://sync/<the folder's>?name=<name>&chat=<the channel's base32>&chat_name=<its name>
//!        [&chat_agents=yes]; `agents=yes`: the channel was open to agents when the link was made, 0.1.13)
//!        [&a=<base32(sealed block)>]: where the device that made the link can be reached, sealed with a key from the
//!        token seed (`crate::reach`, 0.1.13); last, after the channel's fields; one for a folder and its channel
//!        (a link to pair a device, 0.1.13: seedwell://devices/<the same>?name=<the device's name>[&a=…])
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
use crate::codec::{DecodeError, Reader, put_bytes, put_str, put_varint};
use crate::device::{self, DeviceKey};
use crate::ids::FolderId;
use crate::keys::{Access, FolderSecret, KeyError, MAX_LINK_NAME, percent_decode, percent_encode};
use crate::msg::MAX_DEVICE_NAME;
use crate::reach::LinkReach;

const STATEMENT: &str = "seedwell-sync/v2/statement";
const INVITE: &str = "seedwell-sync/v2/invite";
const REDEEM: &str = "seedwell-sync/v2/redeem";
const LINK_CHECK: &str = "seedwell-sync/v2/link";
const LINK_PREFIX: &str = "seedwell://sync/";
const CHAT_PREFIX: &str = "seedwell://chat/";
const DEVICES_PREFIX: &str = "seedwell://devices/";

/// What a link invites to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// A synced folder (`seedwell://sync/`).
    Folder,
    /// A channel (`seedwell://chat/`).
    Channel,
    /// A devices group: pairing a device (`seedwell://devices/`, 0.1.13).
    Devices,
}

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
    /// How the signing device is called here: its device name, and the name of the person using it (empty =
    /// none yet). Signed by that device; its latest (by serial) counts, and older ones are dropped. Every member
    /// shows the same names, also for a device it has never met (conflict copies are named with it).
    Profile {
        /// The device's name, at most [`MAX_DEVICE_NAME`] characters.
        device_name: String,
        /// The user's name, at most [`MAX_DEVICE_NAME`] characters; empty = none.
        user_name: String,
        /// Orders the device's profiles (higher wins).
        serial: u64,
        /// When it was made.
        at_s: u64,
    },
    /// What a channel is (owners only, 0.1.12): [`CHANNEL_AGENTS`] opens it to agents. 0.1.12 signed it when an
    /// agent made the channel and read it as set for good; since 0.1.13 it counts as an [`Body::Agents`] Yes at
    /// serial 0 (so any later Agents statement decides) and is no longer written. A device before 0.1.12 keeps it as
    /// [`Body::Unknown`] and passes it on: to it the channel is an ordinary one.
    Channel {
        /// [`CHANNEL_AGENTS`]; bits this version does not know are kept and mean nothing here.
        flags: u64,
        /// When it was made.
        at_s: u64,
    },
    /// A channel's "Agents: Yes/No" (0.1.13): `open` lets agents on every member device read and post in it, `false`
    /// keeps it from them. Signed by an owner or an admin; the latest (by serial, then time, then signature) of the
    /// owners' and current admins' counts, so it can be switched off and on again. A device before 0.1.13 keeps it as
    /// [`Body::Unknown`] and passes it on (one of 0.1.12 goes by its kind 7 mark alone).
    Agents {
        /// Yes: open to agents.
        open: bool,
        /// Orders the channel's Agents statements (higher wins).
        serial: u64,
        /// When it was made.
        at_s: u64,
    },
    /// "On all my devices" (0.1.13): in a devices group only, a sibling says a folder or channel follows (`on`) or
    /// no longer does. Signed by the device that says it, while it is a member; per target the latest (by serial,
    /// then time, then signature) of a current member counts ([`Membership::follows`]). Anywhere else it means
    /// nothing; a device before 0.1.13 keeps it as [`Body::Unknown`].
    ///
    /// `flags` (0.1.14) are written after `at_s` only when not 0, so a Follow without flags is the 0.1.13 bytes, and
    /// 0.1.13 reads one with flags as the same Follow with those bytes kept after it (its signature checks, it is
    /// passed on as it came): [`FOLLOW_ON_DEMAND`], [`FOLLOW_REMOVED`].
    Follow {
        /// A folder or a channel.
        target: FollowTarget,
        /// Its public key.
        target_pk: [u8; 32],
        /// Its name where it was put on all devices (a hint for the others: the name they give it).
        name: String,
        /// It follows (on) or no longer does (off).
        on: bool,
        /// Orders the target's Follow statements (higher wins).
        serial: u64,
        /// When it was made.
        at_s: u64,
        /// [`FOLLOW_ON_DEMAND`], [`FOLLOW_REMOVED`]; bits this version does not know are kept (0: none, as 0.1.13).
        flags: u64,
    },
    /// "This group moved" (0.1.13): in a devices group only, a member says the group now goes on under a new key
    /// (made after a removal, so the removed device's copy of the old key is worth nothing). Signed by the device that
    /// says it, while it is a member; of the current members' statements, the one with the lowest target group id
    /// counts ([`Membership::moved_to`]), so devices that moved at once settle on one group.
    Moved {
        /// The new group's public key.
        target_pk: [u8; 32],
        /// When it was made.
        at_s: u64,
    },
    /// "The rendezvous I use" (0.1.13): in a devices group only, a device says which rendezvous (SPEC §7G.3) it
    /// publishes its addresses to (empty: none, it turned it off), so a sibling with none takes the same. Signed by
    /// that device while it is a member; its latest (by serial, then signature) counts.
    Rendezvous {
        /// The rendezvous's `https` URL, at most [`MAX_RENDEZVOUS_URL`] bytes; empty: none.
        url: String,
        /// Orders the device's statements (higher wins).
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

/// What a [`Body::Follow`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FollowTarget {
    /// A synced folder.
    Folder,
    /// A channel.
    Channel,
}

impl FollowTarget {
    fn byte(self) -> u8 {
        match self {
            FollowTarget::Folder => 1,
            FollowTarget::Channel => 2,
        }
    }

    fn from_byte(b: u8) -> Option<FollowTarget> {
        match b {
            1 => Some(FollowTarget::Folder),
            2 => Some(FollowTarget::Channel),
            _ => None,
        }
    }
}

/// Longest URL a [`Body::Rendezvous`] carries, in bytes.
pub const MAX_RENDEZVOUS_URL: usize = 200;

/// Longest name a [`Body::Follow`] carries, in bytes.
pub const MAX_FOLLOW_NAME: usize = 255;

/// [`Body::Follow`]'s flag (0.1.14) of a folder that is on demand on the device that put it on all devices: a sibling
/// joins it on demand too, where it can (only the default when it joins; each device switches its own afterwards).
pub const FOLLOW_ON_DEMAND: u64 = 1;

/// [`Body::Follow`]'s flag (0.1.14), with `on` false: the folder's owner removed it from Seedwell, so every sibling
/// removes it too, keeping its files. Without it, off only stops the folder coming to more devices.
pub const FOLLOW_REMOVED: u64 = 2;

/// A Follow's flags after its time (0.1.14): read only when they are there, not 0 and written as this version writes
/// them, so the statement is written back byte for byte; anything else stays in the statement's `extra`.
fn follow_flags(r: &mut Reader<'_>) -> u64 {
    if r.remaining() == 0 {
        return 0;
    }
    let mut peek = r.clone();
    let Ok(v) = peek.varint("follow flags") else { return 0 };
    let mut canonical = Vec::new();
    put_varint(&mut canonical, v);
    if v == 0 || r.remaining() - peek.remaining() != canonical.len() {
        return 0;
    }
    *r = peek;
    v
}

/// [`Body::Channel`]'s flag of a channel open to agents (0.1.12's agent channel): read as an Agents: Yes at serial 0.
pub const CHANNEL_AGENTS: u64 = 1;

/// A name in a [`Body::Profile`]: at most [`MAX_DEVICE_NAME`] characters, no control characters, not blank (a user
/// name may be empty: none yet).
pub fn profile_name_ok(name: &str, may_be_empty: bool) -> bool {
    if name.is_empty() {
        return may_be_empty;
    }
    name.chars().count() <= MAX_DEVICE_NAME && !name.chars().any(char::is_control) && !name.trim().is_empty()
}

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
            Body::Profile { device_name, user_name, serial, at_s } => {
                put_str(&mut out, device_name);
                put_str(&mut out, user_name);
                put_varint(&mut out, *serial);
                put_varint(&mut out, *at_s);
                6
            }
            Body::Channel { flags, at_s } => {
                put_varint(&mut out, *flags);
                put_varint(&mut out, *at_s);
                7
            }
            Body::Agents { open, serial, at_s } => {
                out.push(u8::from(*open));
                put_varint(&mut out, *serial);
                put_varint(&mut out, *at_s);
                8
            }
            Body::Follow { target, target_pk, name, on, serial, at_s, flags } => {
                out.push(target.byte());
                out.extend_from_slice(target_pk);
                put_str(&mut out, name);
                out.push(u8::from(*on));
                put_varint(&mut out, *serial);
                put_varint(&mut out, *at_s);
                // None: exactly the 0.1.13 bytes.
                if *flags != 0 {
                    put_varint(&mut out, *flags);
                }
                9
            }
            Body::Moved { target_pk, at_s } => {
                out.extend_from_slice(target_pk);
                put_varint(&mut out, *at_s);
                10
            }
            Body::Rendezvous { url, serial, at_s } => {
                put_str(&mut out, url);
                put_varint(&mut out, *serial);
                put_varint(&mut out, *at_s);
                11
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
            6 => {
                let device_name = r.str(MAX_DEVICE_NAME * 4, "device name").ok()?;
                let user_name = r.str(MAX_DEVICE_NAME * 4, "user name").ok()?;
                if !profile_name_ok(&device_name, false) || !profile_name_ok(&user_name, true) {
                    return None;
                }
                Body::Profile { device_name, user_name, serial: r.varint("serial").ok()?, at_s: r.varint("time").ok()? }
            }
            7 => Body::Channel { flags: r.varint("channel flags").ok()?, at_s: r.varint("time").ok()? },
            8 => {
                let open = match r.u8("agents").ok()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                Body::Agents { open, serial: r.varint("serial").ok()?, at_s: r.varint("time").ok()? }
            }
            9 => {
                let target = FollowTarget::from_byte(r.u8("target").ok()?)?;
                let target_pk = r.array("target key").ok()?;
                let name = r.str(MAX_FOLLOW_NAME, "name").ok()?;
                if name.chars().any(char::is_control) {
                    return None;
                }
                let on = match r.u8("on").ok()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                let (serial, at_s) = (r.varint("serial").ok()?, r.varint("time").ok()?);
                Body::Follow { target, target_pk, name, on, serial, at_s, flags: follow_flags(r) }
            }
            10 => Body::Moved { target_pk: r.array("target key").ok()?, at_s: r.varint("time").ok()? },
            11 => {
                let url = r.str(MAX_RENDEZVOUS_URL, "url").ok()?;
                if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
                    return None;
                }
                Body::Rendezvous { url, serial: r.varint("serial").ok()?, at_s: r.varint("time").ok()? }
            }
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
            // Only an owner removes, sets the share end or says what the channel is; an admin's grant needs the
            // invitation it admits with.
            Body::Remove { .. } | Body::ShareUntil { .. } | Body::Channel { .. } => self.signer == Signer::Folder,
            Body::Grant { invite: None, .. } => self.signer == Signer::Folder,
            // An owner or an admin (the membership counts only a current admin's).
            Body::Revoke { .. } | Body::Agents { .. } => true,
            // A device names only itself, and says itself what it put on all devices.
            Body::Profile { .. } | Body::Follow { .. } | Body::Moved { .. } | Body::Rendezvous { .. } => matches!(self.signer, Signer::Device(_)),
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

/// A folder or channel that follows ("on all my devices"), as a devices group's [`Body::Follow`] statements say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Follow {
    /// A folder or a channel.
    pub target: FollowTarget,
    /// Its public key.
    pub target_pk: [u8; 32],
    /// Its name where it was put on all devices.
    pub name: String,
    /// It follows.
    pub on: bool,
    /// The device that said it.
    pub by: [u8; 32],
    /// The statement's serial.
    pub serial: u64,
    /// When it was said.
    pub at_s: u64,
    /// The statement's flags ([`FOLLOW_ON_DEMAND`], [`FOLLOW_REMOVED`]).
    pub flags: u64,
}

impl Follow {
    /// On demand on the device that put it on all devices: a sibling joins it on demand where it can.
    pub fn on_demand(&self) -> bool {
        self.flags & FOLLOW_ON_DEMAND != 0
    }

    /// Off because its owner removed it from Seedwell: every sibling removes it too, keeping its files.
    pub fn removed(&self) -> bool {
        !self.on && self.flags & FOLLOW_REMOVED != 0
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
    /// The channel is open to agents: the latest [`Body::Agents`] of an owner or current admin (an owner's
    /// [`CHANNEL_AGENTS`] mark counts as one at serial 0).
    agents: bool,
    /// The highest [`Body::Agents`] serial known.
    agents_serial: u64,
    /// A devices group (0.1.13): an owner's removal is final (no later grant lets the device in again), and
    /// [`Body::Follow`] statements count.
    devices: bool,
    /// Per target, the latest Follow of a current member (devices group only).
    follows: BTreeMap<[u8; 32], Follow>,
    /// Per target, the highest Follow serial known (of any signer).
    follow_serial: BTreeMap<[u8; 32], u64>,
}

impl Membership {
    /// An empty membership of a devices group (0.1.13) whose key is `group_pk` (the user key): removals are final,
    /// and Follow statements count.
    pub fn new_devices(group_pk: [u8; 32]) -> Self {
        Membership { devices: true, ..Membership::new(group_pk) }
    }

    /// A devices group's membership ([`Membership::new_devices`]).
    pub fn is_devices(&self) -> bool {
        self.devices
    }

    /// In a devices group, `device_pk` was removed by an owner, for good.
    pub fn removed_for_good(&self, device_pk: &[u8; 32]) -> bool {
        self.devices && self.removed.contains(device_pk)
    }

    /// What follows in a devices group: per target, the latest Follow of a current member (on or off).
    pub fn follows(&self) -> impl Iterator<Item = &Follow> {
        self.follows.values()
    }

    /// The latest Follow of a current member for one target.
    pub fn follow(&self, target_pk: &[u8; 32]) -> Option<&Follow> {
        self.follows.get(target_pk)
    }

    /// The serial the next Follow for `target_pk` gets (above every one known for it).
    pub fn next_follow_serial(&self, target_pk: &[u8; 32]) -> u64 {
        self.follow_serial.get(target_pk).copied().unwrap_or(0) + 1
    }

    /// Where a devices group moved: every member's (signer, target key) of a [`Body::Moved`] of a current member.
    pub fn moves(&self) -> Vec<([u8; 32], [u8; 32])> {
        if !self.devices {
            return Vec::new();
        }
        let mut out: Vec<([u8; 32], [u8; 32])> = self
            .statements
            .values()
            .filter_map(|st| match (&st.body, st.signer) {
                (Body::Moved { target_pk, .. }, Signer::Device(by)) if self.members.contains_key(&by) => Some((by, *target_pk)),
                _ => None,
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The group a devices group moved to: of the current members' Moved statements whose signer `trusted` accepts,
    /// the one with the lowest target group id (the same everywhere, in any order).
    pub fn moved_to(&self, trusted: impl Fn(&[u8; 32]) -> bool) -> Option<[u8; 32]> {
        self.moves().into_iter().filter(|(by, _)| trusted(by)).map(|(_, t)| t).min_by_key(FolderId::of_public_key)
    }

    /// The rendezvous each current member of a devices group says it uses: (device, url, when), its latest statement
    /// (by serial, then signature) each; an empty url: it uses none.
    pub fn rendezvous(&self) -> Vec<([u8; 32], String, u64)> {
        let mut latest: BTreeMap<[u8; 32], (u64, [u8; 64], String, u64)> = BTreeMap::new();
        if !self.devices {
            return Vec::new();
        }
        for st in self.statements.values() {
            let (Body::Rendezvous { url, serial, at_s }, Signer::Device(by)) = (&st.body, st.signer) else { continue };
            if !self.members.contains_key(&by) {
                continue;
            }
            if latest.get(&by).is_none_or(|(s, sig, _, _)| (*serial, &st.sig) > (*s, sig)) {
                latest.insert(by, (*serial, st.sig, url.clone(), *at_s));
            }
        }
        latest.into_iter().map(|(by, (_, _, url, at))| (by, url, at)).collect()
    }

    /// The serial `device`'s next rendezvous statement gets.
    pub fn next_rendezvous_serial(&self, device: &[u8; 32]) -> u64 {
        self.statements
            .values()
            .filter_map(|st| match (&st.body, st.signer) {
                (Body::Rendezvous { serial, .. }, Signer::Device(by)) if by == *device => Some(*serial),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1
    }

    /// The devices an owner removed from a devices group (for good).
    pub fn removed_devices(&self) -> Vec<[u8; 32]> {
        if self.devices { self.removed.iter().copied().collect() } else { Vec::new() }
    }

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
            agents: false,
            agents_serial: 0,
            devices: false,
            follows: BTreeMap::new(),
            follow_serial: BTreeMap::new(),
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
    ///
    /// A device's [`Body::Profile`] goes in while it is a member, and replaces its older one: one per device.
    pub fn add(&mut self, statements: impl IntoIterator<Item = Statement>) -> bool {
        let (mut by_owner, mut by_device): (Vec<Statement>, Vec<Statement>) =
            statements.into_iter().filter(|st| st.verify(&self.folder_pk)).partition(|st| st.signer == Signer::Folder);
        // Profiles and follows last: a grant in the same batch may make their signer a member.
        by_device.sort_by_key(|st| matches!(st.body, Body::Profile { .. } | Body::Follow { .. } | Body::Moved { .. } | Body::Rendezvous { .. }));
        let before = (self.members.clone(), self.invites.clone(), self.share_until, self.profile_hashes(), self.agents, self.follows.clone(), self.moves(), self.rendezvous());
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
            if let Body::Profile { serial, .. } = st.body {
                if !self.members.contains_key(&pk) {
                    continue;
                }
                match self.profile_statement(&pk) {
                    Some((_, cur)) if (profile_serial(cur), &cur.sig) >= (serial, &st.sig) => continue,
                    Some((h, _)) => {
                        self.statements.remove(&h);
                    }
                    None if self.statements.len() >= MAX_STATEMENTS => continue,
                    None => {}
                }
                more |= self.statements.insert(st.hash(), st).is_none();
                continue;
            }
            // A Follow: in a devices group, from a member (elsewhere as any other device statement: kept from an admin,
            // meaning nothing).
            if matches!(st.body, Body::Follow { .. } | Body::Moved { .. } | Body::Rendezvous { .. }) && self.devices {
                if self.statements.len() < MAX_STATEMENTS && self.role(&pk).is_some() {
                    more |= self.statements.insert(st.hash(), st).is_none();
                }
                continue;
            }
            if self.statements.len() >= MAX_STATEMENTS || !self.role(&pk).is_some_and(Role::admits) {
                continue;
            }
            more |= self.statements.insert(st.hash(), st).is_none();
        }
        if more {
            self.resolve();
        }
        (new || more) && before != (self.members.clone(), self.invites.clone(), self.share_until, self.profile_hashes(), self.agents, self.follows.clone(), self.moves(), self.rendezvous())
    }

    /// The latest profile statement of a device, and its hash.
    fn profile_statement(&self, device_pk: &[u8; 32]) -> Option<([u8; 32], &Statement)> {
        self.statements
            .iter()
            .filter(|(_, st)| st.signer == Signer::Device(*device_pk) && matches!(st.body, Body::Profile { .. }))
            .max_by(|(_, a), (_, b)| (profile_serial(a), &a.sig).cmp(&(profile_serial(b), &b.sig)))
            .map(|(h, st)| (*h, st))
    }

    fn profile_hashes(&self) -> Vec<[u8; 32]> {
        self.statements.iter().filter(|(_, st)| matches!(st.body, Body::Profile { .. })).map(|(h, _)| *h).collect()
    }

    /// How a device calls itself here: (device name, user name; empty = none). Also kept for a device that left,
    /// so its old versions keep their name.
    pub fn profile(&self, device_pk: &[u8; 32]) -> Option<(&str, &str)> {
        match &self.profile_statement(device_pk)?.1.body {
            Body::Profile { device_name, user_name, .. } => Some((device_name.as_str(), user_name.as_str())),
            _ => None,
        }
    }

    /// Every device's latest profile: (device key, device name, user name).
    pub fn profiles(&self) -> impl Iterator<Item = (&[u8; 32], &str, &str)> {
        self.statements.values().filter_map(|st| match (&st.body, &st.signer) {
            (Body::Profile { device_name, user_name, .. }, Signer::Device(pk)) => Some((pk, device_name.as_str(), user_name.as_str())),
            _ => None,
        })
    }

    /// The serial a device's next profile gets.
    pub fn next_profile_serial(&self, device_pk: &[u8; 32]) -> u64 {
        self.profile_statement(device_pk).map_or(0, |(_, st)| profile_serial(st)) + 1
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

    /// The channel is open to agents ("Agents: Yes"): the owners' and current admins' latest [`Body::Agents`], or an
    /// owner's [`CHANNEL_AGENTS`] mark when there is none. No statement: closed.
    pub fn agents(&self) -> bool {
        self.agents
    }

    /// The serial the next [`Body::Agents`] gets (above every one known).
    pub fn next_agents_serial(&self) -> u64 {
        self.agents_serial + 1
    }

    /// The device is a member whose access ended with the folder's sharing.
    pub fn expired(&self, device_pk: &[u8; 32]) -> bool {
        self.members.get(device_pk).is_some_and(|m| !self.active(m))
    }

    /// Every device ever granted a place here, also the ones removed since, in the order they joined: by the earliest
    /// `at_s` of a grant that names it (a later grant does not move it), ties by key. The same on every device that
    /// holds the statements.
    pub fn joined(&self) -> Vec<([u8; 32], u64)> {
        let mut first: BTreeMap<[u8; 32], u64> = BTreeMap::new();
        for st in self.statements.values() {
            if let Body::Grant { device, at_s, .. } = &st.body {
                let at = first.entry(*device).or_insert(*at_s);
                *at = (*at).min(*at_s);
            }
        }
        let mut out: Vec<([u8; 32], u64)> = first.into_iter().collect();
        out.sort_by_key(|&(pk, at)| (at, pk));
        out
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
        // Removed from a devices group: for good, whoever admits.
        if self.removed_for_good(device_pk) {
            return Err(AdmitError::Removed);
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

        // Owners' decisions; Admin (and Owner) roles come only from them. In a devices group an owner's removal is
        // final: no grant, earlier or later, lets that device in again.
        let mut members: BTreeMap<[u8; 32], Member> = BTreeMap::new();
        let mut removed = BTreeSet::new();
        let mut by_invite: Admissions = BTreeMap::new();
        let for_good: BTreeSet<[u8; 32]> = if self.devices {
            self.statements
                .values()
                .filter_map(|st| match (&st.body, st.signer) {
                    (Body::Remove { device, .. }, Signer::Folder) => Some(*device),
                    _ => None,
                })
                .collect()
        } else {
            BTreeSet::new()
        };
        for (device, (st, h)) in &owner {
            if for_good.contains(device) {
                removed.insert(*device);
                continue;
            }
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
        // Agents: Yes/No: the latest of the owners' and current admins' (0.1.12's mark at serial 0, so after any other).
        let mut agents: Option<(AgentsKey, bool)> = None;
        let mut agents_serial = 0;
        for st in self.statements.values() {
            let by_ok = match st.signer {
                Signer::Folder => true,
                Signer::Device(pk) => admins.contains(&pk),
            };
            let said = match st.body {
                Body::Agents { open, serial, at_s } if by_ok => Some(((serial, at_s, st.sig), open)),
                Body::Channel { flags, at_s } if st.signer == Signer::Folder && flags & CHANNEL_AGENTS != 0 => Some(((0, at_s, st.sig), true)),
                _ => None,
            };
            // Every Agents statement kept counts for the next serial (one of an admin demoted since too: it would count
            // again if that device were made an admin again).
            if let Body::Agents { serial, .. } = st.body {
                agents_serial = agents_serial.max(serial);
            }
            if let Some((key, open)) = said
                && agents.as_ref().is_none_or(|(k, _)| key > *k)
            {
                agents = Some((key, open));
            }
        }
        self.agents = agents.is_some_and(|(_, open)| open);
        self.agents_serial = agents_serial;
        // What follows (a devices group): per target, the latest of a current member's.
        let mut follows: BTreeMap<[u8; 32], (FollowKey, Follow)> = BTreeMap::new();
        let mut follow_serial: BTreeMap<[u8; 32], u64> = BTreeMap::new();
        if self.devices {
            for st in self.statements.values() {
                let (Body::Follow { target, target_pk, name, on, serial, at_s, flags }, Signer::Device(by)) = (&st.body, st.signer) else { continue };
                let top = follow_serial.entry(*target_pk).or_insert(0);
                *top = (*top).max(*serial);
                if !self.members.contains_key(&by) {
                    continue;
                }
                let key = (*serial, *at_s, st.sig);
                if follows.get(target_pk).is_none_or(|(k, _)| key > *k) {
                    let f = Follow { target: *target, target_pk: *target_pk, name: name.clone(), on: *on, by, serial: *serial, at_s: *at_s, flags: *flags };
                    follows.insert(*target_pk, (key, f));
                }
            }
        }
        self.follows = follows.into_iter().map(|(k, (_, f))| (k, f)).collect();
        self.follow_serial = follow_serial;
    }
}

/// What orders a target's Follow statements: serial, time, signature.
type FollowKey = (u64, u64, [u8; 64]);

/// The device and serial of an owner's grant or removal.
fn profile_serial(st: &Statement) -> u64 {
    match st.body {
        Body::Profile { serial, .. } => serial,
        _ => 0,
    }
}

fn owner_target(st: &Statement) -> Option<([u8; 32], u64)> {
    match (&st.body, st.signer) {
        (Body::Grant { device, serial, .. } | Body::Remove { device, serial, .. }, Signer::Folder) => Some((*device, *serial)),
        _ => None,
    }
}

/// What orders a channel's Agents statements: serial, time, signature.
type AgentsKey = (u64, u64, [u8; 64]);

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
    /// A channel's link: the channel was open to agents when it was made (`agents=yes`), so agents on the joining
    /// device may use it while it waits to be admitted. A hint only: once admitted, the membership decides.
    pub agents: bool,
    /// Where the device that made the link can be reached (`a=`, sealed; 0.1.13). The joining device dials these
    /// besides discovery. A folder's link for both gives its channel the same.
    pub reach: Option<LinkReach>,
}

impl fmt::Debug for InviteLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InviteLink").field("invite", &self.invite).field("name", &self.name).field("agents", &self.agents).field("reach", &self.reach).finish()
    }
}

impl InviteLink {
    /// The link's text, for a folder.
    pub fn format(&self) -> String {
        self.format_as(LinkKind::Folder)
    }

    /// The link's text, for a channel.
    pub fn format_chat(&self) -> String {
        self.format_as(LinkKind::Channel)
    }

    /// The link's text to pair a device (0.1.13).
    pub fn format_devices(&self) -> String {
        self.format_as(LinkKind::Devices)
    }

    /// The link's text for a folder, a channel or a devices group.
    pub fn format_as(&self, kind: LinkKind) -> String {
        let mut out = self.format_fields(kind);
        self.push_reach(&mut out);
        out
    }

    /// The link without its address block.
    fn format_fields(&self, kind: LinkKind) -> String {
        let prefix = match kind {
            LinkKind::Folder => LINK_PREFIX,
            LinkKind::Channel => CHAT_PREFIX,
            LinkKind::Devices => DEVICES_PREFIX,
        };
        let mut out = format!("{prefix}{}", self.payload());
        if let Some(name) = self.name.as_deref().filter(|n| !n.is_empty()) {
            out.push_str("?name=");
            out.push_str(&percent_encode(name));
        }
        if kind == LinkKind::Channel && self.agents {
            out.push(if out.contains('?') { '&' } else { '?' });
            out.push_str("agents=yes");
        }
        out
    }

    /// A folder's link that also invites to a channel: one link for both. The channel's invitation goes in the
    /// query, `chat=<its payload>` and `chat_name=<its name>`, which a folder link's reader that does not know them
    /// ignores.
    pub fn format_bundle(&self, channel: &InviteLink) -> String {
        let mut out = self.format_fields(LinkKind::Folder);
        out.push(if out.contains('?') { '&' } else { '?' });
        out.push_str("chat=");
        out.push_str(&channel.payload());
        if let Some(name) = channel.name.as_deref().filter(|n| !n.is_empty()) {
            out.push_str("&chat_name=");
            out.push_str(&percent_encode(name));
        }
        if channel.agents {
            out.push_str("&chat_agents=yes");
        }
        self.push_reach(&mut out);
        out
    }

    /// Appends `a=<the sealed address block>` when the link has one.
    fn push_reach(&self, out: &mut String) {
        let Some(r) = &self.reach else { return };
        let sealed = r.seal(&self.token_seed, &self.invite.id);
        if sealed.is_empty() {
            return;
        }
        out.push(if out.contains('?') { '&' } else { '?' });
        out.push_str("a=");
        out.push_str(&base32::encode(&sealed));
    }

    /// The invitation, its signature and the token's seed, with a 2-byte check, in base32.
    fn payload(&self) -> String {
        let mut bytes = Vec::with_capacity(200);
        self.invite.put_unsigned(&mut bytes, false);
        bytes.extend_from_slice(&self.invite.sig);
        bytes.extend_from_slice(&self.token_seed);
        let check = link_check(&bytes);
        bytes.extend_from_slice(&check);
        base32::encode(&bytes)
    }

    /// A link (without its name) from its payload.
    fn from_payload(body: &str) -> Result<InviteLink, KeyError> {
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
        Ok(InviteLink { invite, token_seed, name: None, agents: false, reach: None })
    }

    /// Parses a link. Surrounding whitespace and the `<` `>` quotes mail clients put around links are allowed;
    /// query fields other than `name` (and `agents`, `chat`, `chat_name`, `chat_agents`, `a`) are ignored (later
    /// versions may add some). An address block (`a`) that does not open is left out: the link still joins. A bare key of the earlier test
    /// builds is refused with [`KeyError::OldKey`], a channel's link with [`KeyError::ChannelLink`].
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        match Self::parse_any(text)? {
            (l, LinkKind::Folder) => Ok(l),
            (_, LinkKind::Channel) => Err(KeyError::ChannelLink),
            (_, LinkKind::Devices) => Err(KeyError::DevicesLink),
        }
    }

    /// Parses a channel's link (a folder's is refused with [`KeyError::FolderLink`]).
    pub fn parse_chat(text: &str) -> Result<Self, KeyError> {
        match Self::parse_any(text)? {
            (l, LinkKind::Channel) => Ok(l),
            (_, LinkKind::Folder) => Err(KeyError::FolderLink),
            (_, LinkKind::Devices) => Err(KeyError::DevicesLink),
        }
    }

    /// Parses a link to pair a device (0.1.13; a folder's or a channel's is refused with [`KeyError::FolderLink`] or
    /// [`KeyError::ChannelLink`]).
    pub fn parse_devices(text: &str) -> Result<Self, KeyError> {
        match Self::parse_any(text)? {
            (l, LinkKind::Devices) => Ok(l),
            (_, LinkKind::Folder) => Err(KeyError::FolderLink),
            (_, LinkKind::Channel) => Err(KeyError::ChannelLink),
        }
    }

    /// Parses a folder's, a channel's or a devices group's link.
    pub fn parse_any(text: &str) -> Result<(Self, LinkKind), KeyError> {
        Self::parse_full(text).map(|(l, kind, _)| (l, kind))
    }

    /// Parses a folder's link and the channel invitation it may carry ([`InviteLink::format_bundle`]); a channel's
    /// link is refused with [`KeyError::ChannelLink`]. A damaged channel part makes the whole link damaged.
    pub fn parse_bundle(text: &str) -> Result<(Self, Option<InviteLink>), KeyError> {
        match Self::parse_full(text)? {
            (l, LinkKind::Folder, chat) => Ok((l, chat)),
            (_, LinkKind::Channel, _) => Err(KeyError::ChannelLink),
            (_, LinkKind::Devices, _) => Err(KeyError::DevicesLink),
        }
    }

    fn parse_full(text: &str) -> Result<(Self, LinkKind, Option<InviteLink>), KeyError> {
        let mut text = text.trim();
        if let Some(inner) = text.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
            text = inner.trim();
        }
        if crate::keys::is_old_key(text) {
            return Err(KeyError::OldKey);
        }
        let has = |prefix: &str| text.len() >= prefix.len() && text.is_char_boundary(prefix.len()) && text[..prefix.len()].eq_ignore_ascii_case(prefix);
        let (kind, prefix) = if has(LINK_PREFIX) {
            (LinkKind::Folder, LINK_PREFIX)
        } else if has(CHAT_PREFIX) {
            (LinkKind::Channel, CHAT_PREFIX)
        } else if has(DEVICES_PREFIX) {
            (LinkKind::Devices, DEVICES_PREFIX)
        } else {
            return Err(KeyError::Link);
        };
        let rest = &text[prefix.len()..];
        let (body, query) = rest.split_once('?').unwrap_or((rest, ""));
        let mut link = Self::from_payload(body)?;
        let mut chat: Option<InviteLink> = None;
        let mut chat_name = None;
        let mut chat_agents = false;
        let mut reach = None;
        for field in query.split('&') {
            if let Some(v) = field.strip_prefix("name=") {
                link.name = Some(percent_decode(v)?).filter(|n| !n.is_empty() && n.len() <= MAX_LINK_NAME);
            } else if let Some(v) = field.strip_prefix("chat=")
                && kind == LinkKind::Folder
            {
                let c = Self::from_payload(v)?;
                // The channel is not the folder (a link that says so is not one Seedwell made).
                if c.invite.folder_pk == link.invite.folder_pk {
                    return Err(KeyError::Damaged);
                }
                chat = Some(c);
            } else if let Some(v) = field.strip_prefix("chat_name=") {
                chat_name = Some(percent_decode(v)?).filter(|n| !n.is_empty() && n.len() <= MAX_LINK_NAME);
            } else if let Some(v) = field.strip_prefix("agents=")
                && kind == LinkKind::Channel
            {
                link.agents = v == "yes";
            } else if let Some(v) = field.strip_prefix("chat_agents=") {
                chat_agents = v == "yes";
            } else if let Some(v) = field.strip_prefix("a=") {
                reach = Some(v);
            }
        }
        link.reach = reach.and_then(|v| base32::decode(v).ok()).and_then(|b| LinkReach::open(&b, &link.token_seed, &link.invite.id));
        if let Some(c) = chat.as_mut() {
            c.name = chat_name;
            c.agents = chat_agents;
            c.reach = link.reach.clone();
        }
        Ok((link, kind, chat))
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

    fn profile(d: &DeviceKey, fid: &FolderId, name: &str, user: &str, serial: u64) -> Statement {
        Statement::by_device(d, fid, Body::Profile { device_name: name.into(), user_name: user.into(), serial, at_s: NOW })
    }

    #[test]
    fn devices_are_listed_in_the_order_they_joined_also_after_they_left() {
        let (s, mut m, me) = base();
        let at = |d: &DeviceKey, role: Role, serial: u64, at_s: u64| Statement::by_owner(&s, Body::Grant { device: *d.public_key(), role, serial, at_s, invite: None });
        assert!(m.add([at(&dev(2), Role::ReadWrite, 2, NOW + 20), at(&dev(3), Role::ReadOnly, 3, NOW + 10)]));
        let order = |m: &Membership| m.joined().into_iter().map(|(pk, _)| pk).collect::<Vec<_>>();
        let (p1, p2, p3) = (*me.public_key(), *dev(2).public_key(), *dev(3).public_key());
        assert_eq!(order(&m), [p1, p3, p2], "by the time of the first grant, not the order they arrived in");
        // A role change later does not move a device; one that leaves keeps its place.
        assert!(m.add([at(&dev(3), Role::Admin, 4, NOW + 99)]));
        assert!(m.add([Statement::by_owner(&s, Body::Remove { device: p3, serial: 5, at_s: NOW + 100 })]));
        assert!(m.is_removed(&p3));
        assert_eq!(order(&m), [p1, p3, p2]);
        assert_eq!(m.joined()[1].1, NOW + 10);
    }

    fn agents(open: bool, serial: u64) -> Body {
        Body::Agents { open, serial, at_s: NOW + serial }
    }

    #[test]
    fn agents_yes_or_no_is_the_latest_owners_or_admins_and_can_change() {
        let (s, mut m, me) = base();
        let fid = m.folder_id();
        let (admin, writer) = (dev(2), dev(3));
        assert!(m.add([grant(&s, &admin, Role::Admin, 2), grant(&s, &writer, Role::ReadWrite, 3)]));
        assert!(!m.agents(), "no statement: No");
        assert_eq!(m.next_agents_serial(), 1);
        // The owner's Yes: a change, which round-trips and gives the same from every order.
        let yes = Statement::by_owner(&s, agents(true, 1));
        assert_eq!(Statement::from_bytes(&yes.to_bytes()).unwrap(), yes);
        assert!(m.add([yes.clone()]) && m.agents());
        let mut other = Membership::new(*s.public_key());
        other.add(m.statements().cloned().collect::<Vec<_>>().into_iter().rev());
        assert!(other.agents() && other.role(me.public_key()) == Some(Role::Owner));
        // An admin switches it off (a later serial), and the owner on again.
        assert!(m.add([Statement::by_device(&admin, &fid, agents(false, m.next_agents_serial()))]) && !m.agents());
        assert!(m.add([Statement::by_owner(&s, agents(true, m.next_agents_serial()))]) && m.agents());
        // An older statement arriving late changes nothing.
        assert!(!m.add([Statement::by_device(&admin, &fid, agents(false, 1))]) && m.agents());
        // A writer (no admin) cannot say it: left out.
        assert!(!m.add([Statement::by_device(&writer, &fid, agents(false, 9))]) && m.agents());
        // An admin's statement counts only while it is an admin.
        assert!(m.add([Statement::by_device(&admin, &fid, agents(false, 10))]) && !m.agents());
        assert!(m.add([grant(&s, &admin, Role::ReadWrite, 4)]) && m.agents(), "demoted: the owner's Yes counts again");
        assert_eq!(m.next_agents_serial(), 11, "above the demoted admin's too, so the next one wins if it is promoted again");
        // A body this version cannot read (an open value it does not know) is kept as Unknown and counts for nothing.
        let mut odd = Statement::by_owner(&s, agents(true, 20)).to_bytes();
        odd[2] = 2;
        assert!(matches!(Statement::from_bytes(&odd).unwrap().body, Body::Unknown { kind: 8, .. }));
    }

    #[test]
    fn the_0_1_12_mark_is_a_yes_that_a_later_agents_statement_overrides() {
        let (s, mut m, _) = base();
        let fid = m.folder_id();
        let admin = dev(2);
        assert!(m.add([grant(&s, &admin, Role::Admin, 2)]));
        // Only an owner signs the mark: an admin's is refused at the signature check, as 0.1.12 refuses it.
        let by_admin = Statement::by_device(&admin, &fid, Body::Channel { flags: CHANNEL_AGENTS, at_s: NOW });
        assert!(!by_admin.verify(s.public_key()));
        assert!(!m.add([by_admin]) && !m.agents());
        // Flags this version does not know mean nothing, and are kept as they came.
        let odd = Statement::by_owner(&s, Body::Channel { flags: 0b110, at_s: NOW });
        assert_eq!(Statement::from_bytes(&odd.to_bytes()).unwrap(), odd);
        assert!(!m.add([odd]) && !m.agents(), "kept, and no change");
        // The owner's mark (a 0.1.12 agent channel): Yes.
        assert!(m.add([Statement::by_owner(&s, Body::Channel { flags: CHANNEL_AGENTS, at_s: NOW + 50 })]) && m.agents());
        assert_eq!(m.next_agents_serial(), 1);
        // Any Agents statement comes after it, whatever its time: an admin's No closes the channel.
        assert!(m.add([Statement::by_device(&admin, &fid, Body::Agents { open: false, serial: 1, at_s: NOW })]) && !m.agents());
        // A later mark does not open it again: only an Agents statement does.
        assert!(!m.add([Statement::by_owner(&s, Body::Channel { flags: CHANNEL_AGENTS, at_s: NOW + 99 })]) && !m.agents());
    }

    #[test]
    fn a_member_names_itself_and_its_latest_profile_counts() {
        let (s, mut m, me) = base();
        let fid = m.folder_id();
        let (a, stranger) = (dev(2), dev(3));
        // Every order gives the same names: a profile before its signer's grant waits for the grant.
        let p1 = profile(&a, &fid, "Studio iMac", "", 1);
        assert!(m.add([p1.clone(), grant(&s, &a, Role::ReadWrite, 2)]));
        assert_eq!(m.profile(a.public_key()), Some(("Studio iMac", "")));
        assert_eq!(Statement::from_bytes(&p1.to_bytes()).unwrap(), p1, "it round-trips");
        // A newer one replaces it; an older one, or the same again, changes nothing.
        assert!(m.add([profile(&a, &fid, "Kim's iMac", "kim", 2)]));
        assert!(!m.add([p1.clone()]));
        assert_eq!(m.profile(a.public_key()), Some(("Kim's iMac", "kim")));
        assert_eq!(m.profiles().count(), 1, "one per device");
        assert_eq!(m.next_profile_serial(a.public_key()), 3);
        // Not a member: left out. Nobody names another device: the folder key cannot sign a profile.
        assert!(!m.add([profile(&stranger, &fid, "x", "", 1)]));
        let forged = Statement::by_owner(&s, Body::Profile { device_name: "y".into(), user_name: String::new(), serial: 9, at_s: NOW });
        assert!(!forged.verify(s.public_key()));
        assert!(m.profile(stranger.public_key()).is_none() && m.profile(me.public_key()).is_none());
        // Removed, it keeps its name (its old versions are still called by it).
        assert!(m.add([Statement::by_owner(&s, Body::Remove { device: *a.public_key(), serial: 3, at_s: NOW })]));
        assert_eq!(m.profile(a.public_key()).map(|p| p.0), Some("Kim's iMac"));
    }

    #[test]
    fn a_profile_with_a_bad_name_is_unknown() {
        let fid = FolderId::of_public_key(owner().public_key());
        for (name, user) in [("", ""), ("   ", ""), ("a\u{7}b", ""), ("ok", "\n"), (&"x".repeat(65)[..], "")] {
            let st = profile(&dev(2), &fid, name, user, 1);
            let back = Statement::from_bytes(&st.to_bytes()).unwrap();
            assert!(matches!(back.body, Body::Unknown { kind: 6, .. }), "{name:?} {user:?}");
        }
        assert!(profile_name_ok(&"æ".repeat(64), false) && profile_name_ok("", true));
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
        let link = InviteLink { invite: i, token_seed: [12; 32], name: Some("Bilder & ting/2026 ✓".into()), agents: false, reach: None };
        let text = link.format();
        assert!(text.starts_with("seedwell://sync/"), "{text}");
        assert!(!text[16..].contains(' ') && !text[16..].contains('&') && !text[16..].contains('/'));
        assert_eq!(InviteLink::parse(&text).unwrap(), link);
        assert_eq!(InviteLink::parse(&format!("  <{text}>\n")).unwrap(), link);
        assert_eq!(InviteLink::parse(&text.to_lowercase().replace("seedwell://sync/", "SEEDWELL://SYNC/")).map(|l| l.invite), Ok(i), "either case");
        assert!(text.len() < 340, "{} characters", text.len());
    }

    #[test]
    fn a_channel_link_says_whether_the_channel_is_open_to_agents() {
        let s = owner();
        let ch = InviteLink { invite: invite_by(&s, None, Role::ReadWrite, 1, 15), token_seed: [15; 32], name: Some("Bridge".into()), agents: true, reach: None };
        let text = ch.format_chat();
        assert!(text.ends_with("?name=Bridge&agents=yes"), "{text}");
        assert_eq!(InviteLink::parse_chat(&text).unwrap(), ch);
        let unnamed = InviteLink { name: None, ..ch.clone() };
        assert!(unnamed.format_chat().ends_with("?agents=yes"));
        assert_eq!(InviteLink::parse_chat(&unnamed.format_chat()).unwrap(), unnamed);
        // A No channel's link has no field; anything but `yes` is No.
        let closed = InviteLink { agents: false, ..ch.clone() };
        assert!(!closed.format_chat().contains("agents"));
        assert!(!InviteLink::parse_chat(&text.replace("agents=yes", "agents=1")).unwrap().agents);
        // A folder's link never carries it for itself, only for the channel it bundles.
        let folder = InviteLink { invite: invite_by(&FolderSecret::from_bytes([3; 32]), None, Role::ReadWrite, 1, 16), token_seed: [16; 32], name: None, agents: true, reach: None };
        assert!(!folder.format().contains("agents"));
        let bundle = InviteLink { agents: false, ..folder.clone() }.format_bundle(&ch);
        assert!(bundle.ends_with("&chat_agents=yes"), "{bundle}");
        let (f, c) = InviteLink::parse_bundle(&bundle).unwrap();
        assert_eq!((f.agents, c.map(|c| c.agents)), (false, Some(true)));
    }

    fn reach() -> LinkReach {
        LinkReach {
            device: crate::ids::DeviceId([5; 32]),
            name: "Kristian's Mac mini".into(),
            lan: vec!["192.168.68.57:55799".parse().unwrap(), "[fd00::5]:55799".parse().unwrap()],
            public: Some("84.212.3.4:41000".parse().unwrap()),
            rendezvous: None,
        }
    }

    /// 0.1.12's `parse_full`, as it was released (v0.1.12, members.rs), on today's payload reader (unchanged since):
    /// what an older Seedwell does with a link.
    fn parse_0_1_12(text: &str) -> Result<(InviteLink, LinkKind, Option<InviteLink>), KeyError> {
        let mut text = text.trim();
        if let Some(inner) = text.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
            text = inner.trim();
        }
        if crate::keys::is_old_key(text) {
            return Err(KeyError::OldKey);
        }
        let has = |prefix: &str| text.len() >= prefix.len() && text.is_char_boundary(prefix.len()) && text[..prefix.len()].eq_ignore_ascii_case(prefix);
        let kind = if has(LINK_PREFIX) {
            LinkKind::Folder
        } else if has(CHAT_PREFIX) {
            LinkKind::Channel
        } else {
            return Err(KeyError::Link);
        };
        let rest = &text[if kind == LinkKind::Channel { CHAT_PREFIX.len() } else { LINK_PREFIX.len() }..];
        let (body, query) = rest.split_once('?').unwrap_or((rest, ""));
        let mut link = InviteLink::from_payload(body)?;
        let mut chat: Option<InviteLink> = None;
        let mut chat_name = None;
        for field in query.split('&') {
            if let Some(v) = field.strip_prefix("name=") {
                link.name = Some(percent_decode(v)?).filter(|n| !n.is_empty() && n.len() <= MAX_LINK_NAME);
            } else if let Some(v) = field.strip_prefix("chat=")
                && kind == LinkKind::Folder
            {
                let c = InviteLink::from_payload(v)?;
                if c.invite.folder_pk == link.invite.folder_pk {
                    return Err(KeyError::Damaged);
                }
                chat = Some(c);
            } else if let Some(v) = field.strip_prefix("chat_name=") {
                chat_name = Some(percent_decode(v)?).filter(|n| !n.is_empty() && n.len() <= MAX_LINK_NAME);
            }
        }
        if let Some(c) = chat.as_mut() {
            c.name = chat_name;
        }
        Ok((link, kind, chat))
    }

    #[test]
    fn a_link_carries_where_its_maker_is_sealed_and_older_versions_still_join_with_it() {
        let s = owner();
        let plain = InviteLink { invite: invite_by(&s, None, Role::ReadWrite, 1, 17), token_seed: [17; 32], name: Some("Bilder".into()), agents: false, reach: None };
        let with = InviteLink { reach: Some(reach()), ..plain.clone() };
        let text = with.format();
        assert!(text.starts_with(&plain.format()) && text.contains("&a="), "{text}");
        assert!(!text.contains("192") && !text.contains("55799") && !text.contains("Kristian"), "sealed, not readable: {text}");
        assert_eq!(text, with.format(), "the same text each time");
        assert_eq!(InviteLink::parse(&text).unwrap(), with);
        assert!(text.len() < 500, "{} characters", text.len());
        // An old link still parses, with nothing to dial.
        assert_eq!(InviteLink::parse(&plain.format()).unwrap().reach, None);
        // 0.1.12 reads the new link as the same invitation and ignores the block.
        let (old, kind, chat) = parse_0_1_12(&text).unwrap();
        assert_eq!((old, kind, chat), (plain.clone(), LinkKind::Folder, None));
        // A channel's link, with Agents: Yes.
        let ch = InviteLink { invite: invite_by(&FolderSecret::from_bytes([4; 32]), None, Role::ReadWrite, 1, 18), token_seed: [18; 32], name: Some("Bridge".into()), agents: true, reach: Some(reach()) };
        let ct = ch.format_chat();
        assert!(ct.contains("?name=Bridge&agents=yes&a="), "{ct}");
        assert_eq!(InviteLink::parse_chat(&ct).unwrap(), ch);
        let (old, kind, _) = parse_0_1_12(&ct).unwrap();
        assert_eq!((old, kind), (InviteLink { agents: false, reach: None, ..ch.clone() }, LinkKind::Channel));
        // One block for a folder and its channel: both get it, and 0.1.12 reads both invitations.
        let bundle = with.format_bundle(&InviteLink { reach: None, ..ch.clone() });
        assert_eq!(bundle.matches("a=").count(), 1, "{bundle}");
        assert!(bundle.rsplit('&').next().unwrap().starts_with("a="), "the block comes last: {bundle}");
        let (f, c) = InviteLink::parse_bundle(&bundle).unwrap();
        assert_eq!((f, c), (with.clone(), Some(ch.clone())));
        let (old, _, chat) = parse_0_1_12(&bundle).unwrap();
        assert_eq!((old, chat), (plain.clone(), Some(InviteLink { agents: false, reach: None, ..ch.clone() })));
    }

    #[test]
    fn a_damaged_or_foreign_address_block_is_left_out_and_the_link_still_joins() {
        let s = owner();
        let a = InviteLink { invite: invite_by(&s, None, Role::ReadWrite, 1, 19), token_seed: [19; 32], name: None, agents: false, reach: Some(reach()) };
        let b = InviteLink { invite: invite_by(&s, None, Role::ReadWrite, 1, 20), token_seed: [20; 32], name: None, agents: false, reach: Some(reach()) };
        let (at, bt) = (a.format(), b.format());
        let block = |t: &str| t.split_once("?a=").unwrap().1.to_string();
        // Another link's block (another token seed) does not open under this one.
        let swapped = format!("{}?a={}", at.split_once("?a=").unwrap().0, block(&bt));
        assert_eq!(InviteLink::parse(&swapped).unwrap(), InviteLink { reach: None, ..a.clone() });
        // A changed character, a cut block, garbage: the same.
        let mut bad = block(&at).into_bytes();
        bad[10] = if bad[10] == b'A' { b'B' } else { b'A' };
        let bad = String::from_utf8(bad).unwrap();
        for v in [bad.as_str(), &block(&at)[..20], "", "!!", "AAAA"] {
            let t = format!("{}?a={v}", at.split_once("?a=").unwrap().0);
            assert_eq!(InviteLink::parse(&t).unwrap().reach, None, "{v}");
        }
    }

    #[test]
    fn bad_links_and_old_keys_are_refused() {
        let s = owner();
        let text = InviteLink { invite: invite_by(&s, None, Role::ReadOnly, 0, 13), token_seed: [13; 32], name: None, agents: false, reach: None }.format();
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

    // ------------------------------------------------------------------------------------------ devices groups

    fn follow(d: &DeviceKey, fid: &FolderId, target: [u8; 32], on: bool, serial: u64, at_s: u64) -> Statement {
        let body = Body::Follow { target: FollowTarget::Folder, target_pk: target, name: "Dev".into(), on, serial, at_s, flags: 0 };
        Statement::by_device(d, fid, body)
    }

    /// A devices group of the user key `s` with siblings `me` (and `others`), all owners.
    fn group(s: &FolderSecret, me: &DeviceKey, others: &[&DeviceKey]) -> Membership {
        let mut m = Membership::new_devices(*s.public_key());
        let mut sts = vec![grant(s, me, Role::Owner, 1)];
        sts.extend(others.iter().enumerate().map(|(i, d)| grant(s, d, Role::Owner, 2 + i as u64)));
        assert!(m.add(sts));
        m
    }

    #[test]
    fn a_follow_round_trips_and_a_bad_body_is_unknown() {
        let s = owner();
        let fid = FolderId::of_public_key(s.public_key());
        let st = Statement::by_device(&dev(2), &fid, Body::Follow { target: FollowTarget::Channel, target_pk: [5; 32], name: "Seedwell Development".into(), on: true, serial: 3, at_s: NOW, flags: 0 });
        let back = Statement::from_bytes(&st.to_bytes()).unwrap();
        assert_eq!(back, st);
        assert!(back.verify(s.public_key()));
        // Only a device says what it follows: the group key cannot.
        let by_key = Statement::by_owner(&s, Body::Follow { target: FollowTarget::Folder, target_pk: [5; 32], name: String::new(), on: true, serial: 1, at_s: NOW, flags: 0 });
        assert!(!by_key.verify(s.public_key()));
        // A target or an on value this version does not know: kept as Unknown, passed on byte for byte.
        let mut odd = st.to_bytes();
        odd[2] = 3;
        let unknown = Statement::from_bytes(&odd).unwrap();
        assert!(matches!(unknown.body, Body::Unknown { kind: 9, .. }));
        assert_eq!(unknown.to_bytes(), odd);
    }

    /// 0.1.14's flags to 0.1.13 and back: a Follow without flags is the 0.1.13 bytes; one with flags is, to 0.1.13,
    /// the same Follow with the flags' bytes after it (kept in `extra`, so the signature checks and the statement goes
    /// on as it came), and 0.1.13's Follow reads here as one without flags.
    #[test]
    fn follow_flags_are_read_by_0_1_13_as_the_same_follow_with_bytes_after_it() {
        let s = owner();
        let fid = FolderId::of_public_key(s.public_key());
        let follow = |on, serial, flags| Body::Follow { target: FollowTarget::Folder, target_pk: [5; 32], name: "Photos".into(), on, serial, at_s: NOW, flags };
        let body = |flags| follow(true, 4, flags);
        let plain = Statement::by_device(&dev(2), &fid, body(0));
        let flagged = Statement::by_device(&dev(2), &fid, body(FOLLOW_ON_DEMAND));
        let (_, plain_body) = Statement::body_bytes(&plain.body, &[]);
        let (_, flagged_body) = Statement::body_bytes(&flagged.body, &[]);
        assert_eq!(flagged_body, [plain_body.as_slice(), &[1]].concat(), "the 0.1.13 body, then the flags");
        // Read here: each as it was made.
        for st in [&plain, &flagged] {
            let back = Statement::from_bytes(&st.to_bytes()).unwrap();
            assert_eq!(&back, st);
            assert!(back.extra.is_empty() && back.verify(s.public_key()));
        }
        // Read by 0.1.13 (its reader knows the fields up to the time, and keeps what follows): the same Follow, no
        // flags, the byte kept, the signature good, written back byte for byte.
        let old = Statement { body: body(0), signer: flagged.signer, sig: flagged.sig, extra: vec![1] };
        assert!(old.verify(s.public_key()));
        assert_eq!(old.to_bytes(), flagged.to_bytes());
        let mut m = group(&s, &dev(1), &[&dev(2)]);
        m.add([old]);
        assert_eq!(m.follow(&[5; 32]).map(|f| (f.on, f.flags)), Some((true, 0)), "0.1.13 follows it, in full");
        // Flags this version does not know are kept; a zero or a padded varint is not read as flags but kept after.
        let future = Statement::by_device(&dev(2), &fid, body(FOLLOW_ON_DEMAND | 64));
        assert_eq!(Statement::from_bytes(&future.to_bytes()).unwrap(), future);
        for tail in [vec![0u8], vec![0x81, 0x00]] {
            let odd = Statement { body: body(0), signer: plain.signer, sig: plain.sig, extra: tail.clone() };
            let back = Statement::from_bytes(&odd.to_bytes()).unwrap();
            assert_eq!((back.to_bytes(), back.extra), (odd.to_bytes(), tail));
        }
        // Removed: only an off Follow with the flag.
        let mut g = group(&s, &dev(1), &[&dev(2)]);
        g.add([Statement::by_device(&dev(2), &fid, follow(false, 5, FOLLOW_REMOVED))]);
        assert!(g.follow(&[5; 32]).unwrap().removed());
        g.add([Statement::by_device(&dev(2), &fid, follow(true, 6, FOLLOW_REMOVED | FOLLOW_ON_DEMAND))]);
        let f = g.follow(&[5; 32]).unwrap();
        assert!(f.on && f.on_demand() && !f.removed());
    }

    #[test]
    fn a_removal_in_a_devices_group_is_final_and_in_a_folder_it_is_not() {
        let s = owner();
        let (me, lost) = (dev(10), dev(11));
        let removal = Statement::by_owner(&s, Body::Remove { device: *lost.public_key(), serial: 5, at_s: NOW });
        let again = grant(&s, &lost, Role::Owner, 6);
        // A folder: the owner's later grant lets it back in.
        let mut folder = Membership::new(*s.public_key());
        folder.add([grant(&s, &me, Role::Owner, 1), grant(&s, &lost, Role::Owner, 2), removal.clone(), again.clone()]);
        assert_eq!(folder.role(lost.public_key()), Some(Role::Owner));
        // A devices group: removed for good, whatever comes later and in any order.
        let mut g = group(&s, &me, &[&lost]);
        assert!(g.add([removal.clone()]));
        assert!(g.is_removed(lost.public_key()) && g.removed_for_good(lost.public_key()));
        assert!(!g.add([again.clone()]), "its own grant (it still holds the key) changes nothing");
        assert_eq!(g.role(lost.public_key()), None);
        let mut other = Membership::new_devices(*s.public_key());
        other.add([again, removal, grant(&s, &me, Role::Owner, 1)]);
        assert_eq!((other.role(lost.public_key()), other.role(me.public_key())), (None, Some(Role::Owner)));
        // Nobody admits it with a link either.
        let i = invite_by(&s, Some(&me), Role::ReadWrite, 1, 20);
        assert_eq!(g.check_admit(me.public_key(), &i, lost.public_key(), NOW), Err(AdmitError::Removed));
    }

    #[test]
    fn the_latest_follow_of_a_current_member_counts() {
        let s = owner();
        let (a, b) = (dev(10), dev(11));
        let mut g = group(&s, &a, &[&b]);
        let gid = g.folder_id();
        let t = [7; 32];
        assert!(g.follow(&t).is_none() && g.next_follow_serial(&t) == 1);
        assert!(g.add([follow(&a, &gid, t, true, 1, NOW)]));
        assert!(g.follow(&t).is_some_and(|f| f.on && f.by == *a.public_key() && f.name == "Dev"));
        // B turns it off (a higher serial): off, whatever the times say.
        assert!(g.add([follow(&b, &gid, t, false, 2, NOW - 100)]));
        assert!(g.follow(&t).is_some_and(|f| !f.on));
        // An older statement arriving late changes nothing; the same serial goes by time.
        assert!(!g.add([follow(&a, &gid, t, true, 1, NOW + 50)]));
        assert!(g.add([follow(&a, &gid, t, true, 2, NOW + 1)]) && g.follow(&t).is_some_and(|f| f.on));
        assert_eq!(g.next_follow_serial(&t), 3);
        // Every order gives the same.
        let mut other = Membership::new_devices(*s.public_key());
        other.add(g.statements().cloned().collect::<Vec<_>>().into_iter().rev());
        assert_eq!(other.follow(&t), g.follow(&t));
        // A stranger's is left out.
        assert!(!g.add([follow(&dev(12), &gid, t, false, 9, NOW)]));
        // In a folder a follow means nothing.
        let mut folder = Membership::new(*s.public_key());
        folder.add([grant(&s, &a, Role::Owner, 1)]);
        folder.add([follow(&a, &gid, t, true, 1, NOW)]);
        assert_eq!(folder.follows().count(), 0);
    }

    #[test]
    fn a_follow_from_a_removed_member_is_ignored() {
        let s = owner();
        let (a, thief) = (dev(10), dev(11));
        let mut g = group(&s, &a, &[&thief]);
        let gid = g.folder_id();
        let t = [8; 32];
        g.add([follow(&a, &gid, t, true, 1, NOW), follow(&thief, &gid, t, false, 9, NOW)]);
        assert!(g.follow(&t).is_some_and(|f| !f.on), "while it is a member, its later No counts");
        assert!(g.add([Statement::by_owner(&s, Body::Remove { device: *thief.public_key(), serial: 9, at_s: NOW })]));
        assert!(g.follow(&t).is_some_and(|f| f.on && f.by == *a.public_key()), "removed: the honest device's Yes again");
        assert_eq!(g.next_follow_serial(&t), 10, "above its statement too");
        assert!(!g.add([follow(&thief, &gid, t, false, 20, NOW)]), "and nothing new of it goes in");
    }

    #[test]
    fn a_group_moves_to_the_lowest_target_of_its_current_members() {
        let s = owner();
        let (a, b, thief) = (dev(10), dev(11), dev(12));
        let mut g = group(&s, &a, &[&b, &thief]);
        let gid = g.folder_id();
        let moved = |d: &DeviceKey, t: u8| Statement::by_device(d, &gid, Body::Moved { target_pk: [t; 32], at_s: NOW });
        let st = moved(&a, 1);
        assert_eq!(Statement::from_bytes(&st.to_bytes()).unwrap(), st, "it round-trips");
        assert!(!Statement::by_owner(&s, Body::Moved { target_pk: [1; 32], at_s: NOW }).verify(s.public_key()), "a device says it, not the key");
        assert_eq!(g.moved_to(|_| true), None);
        assert!(g.add([moved(&a, 1), moved(&b, 2)]));
        let lowest = [[1u8; 32], [2u8; 32]].into_iter().min_by_key(FolderId::of_public_key);
        assert_eq!(g.moved_to(|_| true), lowest, "two moves at once: the lowest group id, everywhere");
        let mut other = Membership::new_devices(*s.public_key());
        other.add(g.statements().cloned().collect::<Vec<_>>().into_iter().rev());
        assert_eq!(other.moved_to(|_| true), lowest, "in any order");
        assert_eq!(g.moved_to(|by| *by == *b.public_key()), Some([2; 32]), "only signers the caller trusts");
        // A removed member's move counts for nothing, also one it made before its removal.
        assert!(g.add([moved(&thief, 0)]));
        assert!(g.moves().iter().any(|(by, _)| by == thief.public_key()));
        assert!(g.add([Statement::by_owner(&s, Body::Remove { device: *thief.public_key(), serial: 9, at_s: NOW })]));
        assert!(g.moves().iter().all(|(by, _)| by != thief.public_key()));
        assert_eq!(g.removed_devices(), vec![*thief.public_key()]);
        assert!(!g.add([moved(&thief, 3)]), "and a later one does not go in");
        // In a folder a move means nothing.
        let mut folder = Membership::new(*s.public_key());
        folder.add([grant(&s, &a, Role::Owner, 1), moved(&a, 1)]);
        assert!(folder.moves().is_empty());
    }

    #[test]
    fn each_member_says_its_rendezvous_and_its_latest_counts() {
        let s = owner();
        let (a, b, gone) = (dev(10), dev(11), dev(12));
        let mut g = group(&s, &a, &[&b, &gone]);
        let gid = g.folder_id();
        let rv = |d: &DeviceKey, url: &str, serial: u64| Statement::by_device(d, &gid, Body::Rendezvous { url: url.into(), serial, at_s: NOW + serial });
        let st = rv(&a, "https://seedwell-rendezvous.kim.workers.dev", 1);
        assert_eq!(Statement::from_bytes(&st.to_bytes()).unwrap(), st, "it round-trips");
        assert!(g.add([st, rv(&gone, "https://thief.example", 1)]));
        assert_eq!(g.next_rendezvous_serial(a.public_key()), 2);
        // A turns it off (an empty URL, later): that counts for A; the others' stay theirs.
        assert!(g.add([rv(&a, "", 2), rv(&a, "https://old.example", 1)]));
        let said = g.rendezvous();
        assert!(said.contains(&(*a.public_key(), String::new(), NOW + 2)), "{said:?}");
        assert!(!said.iter().any(|(by, _, _)| by == b.public_key()), "B said nothing");
        // A removed member's counts for nothing.
        g.add([Statement::by_owner(&s, Body::Remove { device: *gone.public_key(), serial: 9, at_s: NOW })]);
        assert!(!g.rendezvous().iter().any(|(by, _, _)| by == gone.public_key()));
        // A URL with spaces, or too long, is a body this version cannot read.
        let bad = rv(&b, "https://a b", 1).to_bytes();
        assert!(matches!(Statement::from_bytes(&bad).unwrap().body, Body::Unknown { kind: 11, .. }));
    }

    #[test]
    fn a_pairing_link_is_its_own_kind() {
        let s = owner();
        let me = dev(10);
        let i = invite_by(&s, Some(&me), Role::ReadWrite, 1, 21);
        let link = InviteLink { invite: i, token_seed: [21; 32], name: Some("Kim's MacBook".into()), agents: true, reach: Some(reach()) };
        let text = link.format_devices();
        assert!(text.starts_with("seedwell://devices/") && !text.contains("agents="), "{text}");
        let back = InviteLink::parse_devices(&text).unwrap();
        assert_eq!((back.invite, back.name.as_deref(), back.reach), (i, Some("Kim's MacBook"), Some(reach())));
        assert_eq!(InviteLink::parse_any(&text).map(|(_, k)| k), Ok(LinkKind::Devices));
        // Each reader takes only its own kind.
        assert_eq!(InviteLink::parse(&text), Err(KeyError::DevicesLink));
        assert_eq!(InviteLink::parse_chat(&text), Err(KeyError::DevicesLink));
        assert_eq!(InviteLink::parse_bundle(&text), Err(KeyError::DevicesLink));
        assert_eq!(InviteLink::parse_devices(&link.format()), Err(KeyError::FolderLink));
        assert_eq!(InviteLink::parse_devices(&link.format_chat()), Err(KeyError::ChannelLink));
        // A version before 0.1.13 does not know the prefix: not a Seedwell link to it.
        assert_eq!(parse_0_1_12(&text).map(|_| ()), Err(KeyError::Link));
    }
}
