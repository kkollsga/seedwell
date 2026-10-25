//! Private messages between two devices that share a channel (SPEC §7G.7 *Direct messages*): end-to-end encrypted
//! to the recipient, signed by the author, carried and kept by the members of the channels they share, who can
//! pass them on but not read them.
//!
//! Each device has an X25519 key pair for this, derived from its device key ([`DeviceKey::derive`], so nothing
//! more is stored). Its public half travels in a [`DirectKey`] record signed by the device key, which devices pass
//! on to the members of the channels they share.
//!
//! ```text
//! direct key = device pk(32) ‖ x25519 pk(32) ‖ at_ms varint ‖ signature(64)
//!   signed   = "seedwell-chat/v1/direct-key" ‖ device pk ‖ x25519 pk ‖ at_ms (u64 BE)
//! direct     = fields (varint length ‖ bytes) ‖ signature(64)
//!   fields   = id(16) ‖ author pk(32) ‖ recipient pk(32) ‖ seq varint ‖ at_ms varint ‖ ephemeral x25519 pk(32)
//!              ‖ sealed bytes ‖ (later fields)
//!   signed   = "seedwell-chat/v1/direct" ‖ fields (with their length)
//!   sealed   = ChaCha20-Poly1305(key, nonce 0, aad = "seedwell-chat/v1/direct" ‖ id ‖ author ‖ recipient, body)
//!   key      = HKDF-SHA256(salt "seedwell-chat/v1/direct", X25519(ephemeral, recipient's x25519),
//!                          info = ephemeral pk ‖ recipient's x25519 pk ‖ author ‖ recipient ‖ id)
//!   body     = has_reply u8 [‖ reply_to id(16)] ‖ agent str ‖ kind u8 ‖ text bytes ‖ (later fields)
//! ```
//!
//! A new ephemeral key per message (so the key is never reused, and the nonce can be fixed). The author keeps its
//! own copy in the clear on its own disk; nobody else can open a message but its recipient. `seq` counts the
//! author's private messages (to anyone), so a summary of what a device holds is a few ranges per author.

use std::fmt;

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf::{HKDF_SHA256, KeyType, Salt};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::chat::{ChatError, MAX_AGENT, MAX_FIELDS, MessageKind, check_agent, check_body};
use crate::codec::{DecodeError, Reader, put_bool, put_bytes, put_str, put_varint};
use crate::device::{self, DeviceKey};

const KEY_RECORD: &str = "seedwell-chat/v1/direct-key";
const DIRECT: &str = "seedwell-chat/v1/direct";
const DERIVE: &str = "seedwell-chat/v1/x25519";

/// This device's X25519 secret for private messages (derived from its device key).
pub fn secret_of(key: &DeviceKey) -> [u8; 32] {
    key.derive(DERIVE)
}

/// The X25519 public key of a secret.
pub fn public_of(secret: &[u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(*secret)).to_bytes()
}

/// A device's X25519 public key, signed by its device key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DirectKey {
    /// The device.
    pub device: [u8; 32],
    /// Its X25519 public key.
    pub x25519: [u8; 32],
    /// When it was signed (a newer record replaces an older one).
    pub at_ms: u64,
    /// The device's signature.
    pub sig: [u8; 64],
}

impl fmt::Debug for DirectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DirectKey({})", crate::ids::DeviceId::of_public_key(&self.device))
    }
}

impl DirectKey {
    fn signed_bytes(device: &[u8; 32], x25519: &[u8; 32], at_ms: u64) -> Vec<u8> {
        [KEY_RECORD.as_bytes(), device, x25519, &at_ms.to_be_bytes()].concat()
    }

    /// This device's record.
    pub fn of(key: &DeviceKey, at_ms: u64) -> DirectKey {
        let x25519 = public_of(&secret_of(key));
        let sig = key.sign(&Self::signed_bytes(key.public_key(), &x25519, at_ms));
        DirectKey { device: *key.public_key(), x25519, at_ms, sig }
    }

    /// The device signed it.
    pub fn verify(&self) -> bool {
        device::verify(&self.device, &Self::signed_bytes(&self.device, &self.x25519, self.at_ms), &self.sig)
    }

    /// The wire form.
    pub fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.device);
        out.extend_from_slice(&self.x25519);
        put_varint(out, self.at_ms);
        out.extend_from_slice(&self.sig);
    }

    /// Reads the wire form (checked by [`DirectKey::verify`]).
    pub fn read(r: &mut Reader<'_>) -> Result<DirectKey, DecodeError> {
        Ok(DirectKey { device: r.array("device key")?, x25519: r.array("x25519 key")?, at_ms: r.varint("key time")?, sig: r.array("key signature")? })
    }
}

/// What a private message says (the part only its recipient reads).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectBody {
    /// The message it answers.
    pub reply_to: Option<[u8; 16]>,
    /// The agent that posted it (the control client's name), if one did.
    pub agent: Option<String>,
    /// Text or JSON (a later kind is kept, not shown).
    pub kind: MessageKind,
    /// The body.
    pub body: Vec<u8>,
}

impl DirectBody {
    /// Its bytes (what is sealed; the author's own copy keeps them too).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 64);
        match &self.reply_to {
            Some(r) => {
                put_bool(&mut out, true);
                out.extend_from_slice(r);
            }
            None => put_bool(&mut out, false),
        }
        put_str(&mut out, self.agent.as_deref().unwrap_or(""));
        out.push(self.kind.byte());
        put_bytes(&mut out, &self.body);
        out
    }

    /// From its bytes.
    pub fn from_bytes(b: &[u8]) -> Result<DirectBody, DecodeError> {
        let r = &mut Reader::new(b);
        let reply_to = if r.bool("has reply")? { Some(r.array("reply to")?) } else { None };
        let agent = r.str(MAX_AGENT * 4, "agent")?;
        let agent = (!agent.is_empty()).then_some(agent);
        let kind = MessageKind::from_byte(r.u8("message kind")?);
        let body = r.bytes(crate::chat::MAX_JSON, "message body")?.to_vec();
        if check_agent(agent.as_deref()).is_err() || check_body(kind, &body).is_err() {
            return Err(DecodeError::Invalid("message body"));
        }
        Ok(DirectBody { reply_to, agent, kind, body })
    }

    /// The text (text and JSON).
    pub fn text(&self) -> Option<&str> {
        match self.kind {
            MessageKind::Text | MessageKind::Json => std::str::from_utf8(&self.body).ok(),
            _ => None,
        }
    }
}

/// A private message on the wire: readable only by its recipient, signed by its author.
#[derive(Clone, PartialEq, Eq)]
pub struct DirectMessage {
    /// Random; names the message (replies point at it).
    pub id: [u8; 16],
    /// The device that wrote it.
    pub author: [u8; 32],
    /// The device it is for.
    pub recipient: [u8; 32],
    /// The author's count of its private messages: 1, 2, …
    pub seq: u64,
    /// When it was written, Unix milliseconds (the author's clock).
    pub at_ms: u64,
    /// The ephemeral X25519 public key it was sealed with.
    pub ephemeral: [u8; 32],
    /// The sealed [`DirectBody`].
    pub sealed: Vec<u8>,
    /// Fields a later version appended (kept, signed, passed on).
    pub extra: Vec<u8>,
    /// The author's signature.
    pub sig: [u8; 64],
}

impl fmt::Debug for DirectMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DirectMessage({}, seq {}, {} sealed bytes)", crate::members::hex(&self.id), self.seq, self.sealed.len())
    }
}

/// What the author's service makes a private message from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectDraft {
    /// Random id.
    pub id: [u8; 16],
    /// The author's next private sequence number.
    pub seq: u64,
    /// Now, Unix milliseconds.
    pub at_ms: u64,
    /// What it says.
    pub body: DirectBody,
    /// A fresh random secret for the ephemeral key.
    pub ephemeral_secret: [u8; 32],
}

struct Len32;

impl KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

fn aad(id: &[u8; 16], author: &[u8; 32], recipient: &[u8; 32]) -> Vec<u8> {
    [DIRECT.as_bytes(), id, author, recipient].concat()
}

/// The message key, from the X25519 shared secret (`None` for a key that gives no shared secret).
fn message_key(shared: &x25519_dalek::SharedSecret, ephemeral: &[u8; 32], recipient_x: &[u8; 32], author: &[u8; 32], recipient: &[u8; 32], id: &[u8; 16]) -> Option<LessSafeKey> {
    if !shared.was_contributory() {
        return None;
    }
    let prk = Salt::new(HKDF_SHA256, DIRECT.as_bytes()).extract(shared.as_bytes());
    let info: [&[u8]; 5] = [ephemeral, recipient_x, author, recipient, id];
    let okm = prk.expand(&info, Len32).ok()?;
    let mut k = [0u8; 32];
    okm.fill(&mut k).ok()?;
    Some(LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &k).ok()?))
}

impl DirectMessage {
    /// Seals a draft for `recipient` (its device key and its X25519 key) and signs it as `key`'s device.
    pub fn seal(key: &DeviceKey, recipient: &[u8; 32], recipient_x: &[u8; 32], d: DirectDraft) -> Result<DirectMessage, ChatError> {
        check_body(d.body.kind, &d.body.body)?;
        check_agent(d.body.agent.as_deref())?;
        let eph = StaticSecret::from(d.ephemeral_secret);
        let ephemeral = PublicKey::from(&eph).to_bytes();
        let shared = eph.diffie_hellman(&PublicKey::from(*recipient_x));
        let author = *key.public_key();
        let k = message_key(&shared, &ephemeral, recipient_x, &author, recipient, &d.id).ok_or(ChatError::Key)?;
        let mut sealed = d.body.to_bytes();
        k.seal_in_place_append_tag(Nonce::assume_unique_for_key([0; 12]), Aad::from(aad(&d.id, &author, recipient)), &mut sealed).map_err(|_| ChatError::Key)?;
        let mut m = DirectMessage { id: d.id, author, recipient: *recipient, seq: d.seq, at_ms: d.at_ms, ephemeral, sealed, extra: Vec::new(), sig: [0; 64] };
        m.sig = key.sign(&m.signed_bytes());
        Ok(m)
    }

    fn fields(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.sealed.len() + 160);
        out.extend_from_slice(&self.id);
        out.extend_from_slice(&self.author);
        out.extend_from_slice(&self.recipient);
        put_varint(&mut out, self.seq);
        put_varint(&mut out, self.at_ms);
        out.extend_from_slice(&self.ephemeral);
        put_bytes(&mut out, &self.sealed);
        out.extend_from_slice(&self.extra);
        out
    }

    fn signed_bytes(&self) -> Vec<u8> {
        let mut out = DIRECT.as_bytes().to_vec();
        put_bytes(&mut out, &self.fields());
        out
    }

    /// The author signed it (anyone can check; only the recipient can read it).
    pub fn verify(&self) -> bool {
        device::verify(&self.author, &self.signed_bytes(), &self.sig)
    }

    /// Opens it with the recipient's X25519 secret: `None` when it is not this device's, or was changed.
    pub fn open(&self, my_secret: &[u8; 32]) -> Option<DirectBody> {
        let me = StaticSecret::from(*my_secret);
        let my_x = PublicKey::from(&me).to_bytes();
        let shared = me.diffie_hellman(&PublicKey::from(self.ephemeral));
        let k = message_key(&shared, &self.ephemeral, &my_x, &self.author, &self.recipient, &self.id)?;
        let mut buf = self.sealed.clone();
        let plain = k.open_in_place(Nonce::assume_unique_for_key([0; 12]), Aad::from(aad(&self.id, &self.author, &self.recipient)), &mut buf).ok()?;
        DirectBody::from_bytes(plain).ok()
    }

    /// The wire form.
    pub fn put(&self, out: &mut Vec<u8>) {
        put_bytes(out, &self.fields());
        out.extend_from_slice(&self.sig);
    }

    /// The wire form, on its own.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.sealed.len() + 240);
        self.put(&mut out);
        out
    }

    /// Reads one message (checked by [`DirectMessage::verify`]).
    pub fn read(r: &mut Reader<'_>) -> Result<DirectMessage, DecodeError> {
        let fields = r.bytes(MAX_FIELDS, "direct message")?;
        let sig = r.array("direct signature")?;
        let f = &mut Reader::new(fields);
        let id = f.array("message id")?;
        let author = f.array("author")?;
        let recipient = f.array("recipient")?;
        let seq = f.varint("message seq")?;
        let at_ms = f.varint("message time")?;
        let ephemeral = f.array("ephemeral key")?;
        let sealed = f.bytes(MAX_FIELDS, "sealed body")?.to_vec();
        let extra = f.take(f.remaining(), "message extra")?.to_vec();
        Ok(DirectMessage { id, author, recipient, seq, at_ms, ephemeral, sealed, extra, sig })
    }

    /// Reads a message from its own bytes.
    pub fn from_bytes(b: &[u8]) -> Result<DirectMessage, DecodeError> {
        DirectMessage::read(&mut Reader::new(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: &str) -> DirectBody {
        DirectBody { reply_to: Some([4; 16]), agent: Some("claude".into()), kind: MessageKind::Text, body: text.as_bytes().to_vec() }
    }

    fn draft(text: &str, eph: u8) -> DirectDraft {
        DirectDraft { id: [eph; 16], seq: 1, at_ms: 1_800_000_000_000, body: body(text), ephemeral_secret: [eph; 32] }
    }

    #[test]
    fn only_the_recipient_reads_it_and_nobody_can_change_it() {
        let (a, b, c) = (DeviceKey::from_seed(&[1; 32]), DeviceKey::from_seed(&[2; 32]), DeviceKey::from_seed(&[3; 32]));
        let bk = DirectKey::of(&b, 5);
        assert!(bk.verify());
        let m = DirectMessage::seal(&a, b.public_key(), &bk.x25519, draft("hemmelig", 9)).unwrap();
        let back = DirectMessage::from_bytes(&m.to_bytes()).unwrap();
        assert_eq!(back, m);
        assert!(back.verify());
        assert_eq!(back.open(&secret_of(&b)), Some(body("hemmelig")));
        assert_eq!(back.open(&secret_of(&c)), None, "a member that passes it on cannot read it");
        assert_eq!(back.open(&secret_of(&a)), None, "nor its author (who keeps its own copy)");
        assert!(!m.to_bytes().windows(8).any(|w| w == b"hemmelig"), "sealed on the wire");
        let mut x = m.clone();
        x.sealed[0] ^= 1;
        assert!(!x.verify() && x.open(&secret_of(&b)).is_none(), "tampering fails both checks");
        let mut y = m.clone();
        y.recipient = *c.public_key();
        assert!(!y.verify());
        // A key record that someone else signed is no good.
        let mut forged = bk;
        forged.x25519 = DirectKey::of(&c, 5).x25519;
        assert!(!forged.verify());
        // A new ephemeral key per message: the same text seals differently.
        let m2 = DirectMessage::seal(&a, b.public_key(), &bk.x25519, draft("hemmelig", 10)).unwrap();
        assert_ne!(m2.sealed, m.sealed);
        let mut out = Vec::new();
        bk.put(&mut out);
        assert_eq!(DirectKey::read(&mut Reader::new(&out)), Ok(bk));
    }
}
