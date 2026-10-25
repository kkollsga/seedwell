//! Channel messages (SPEC §7G.7 *Channels*): what members of a channel post, signed by the device that posted it,
//! and the summary two devices compare to send each other what the other lacks.
//!
//! A channel is a folder without files: its own key pair, its own [`crate::members::Membership`], shared by
//! `seedwell://chat/` links. Owners, admins and Read & write members post; Read only members read.
//!
//! ```text
//! message = fields (varint length ‖ bytes) ‖ signature(64)
//!   fields = id(16) ‖ author pk(32) ‖ seq varint ‖ at_ms varint ‖ has_reply u8 [‖ reply_to id(16)]
//!            ‖ has_topic u8 [‖ topic id(16)] ‖ agent str (empty = none) ‖ kind u8 ‖ body bytes ‖ (later fields)
//!   signed = "seedwell-chat/v1/message" ‖ channel id ‖ fields (with their length)
//!   kind   = 1 text (UTF-8, ≤ 16 KiB) | 2 json (UTF-8, ≤ 64 KiB) | 3 topic (UTF-8 title, ≤ 200 characters: the
//!            message's id names a new topic) | other: kept and passed on, not shown
//! ```
//!
//! **Topics**: a channel holds named topics; a message with `topic` belongs to it, one without (or naming a topic
//! this device does not know) to General.
//!
//! `at_ms` is milliseconds, so a quick answer still comes after its question. `seq` counts an author's messages in a channel (1, 2, …): what a device holds is a set of ranges per author
//! ([`AuthorHave`]), so two devices tell each other what they have in a few bytes. `id` is random and names a
//! message in replies. The agent label (the control client that posted it, set by the posting device's service)
//! is inside the signed bytes, so no one along the way can drop or change it. Fields a later version appends are
//! kept in `extra` and passed on, as for statements.

use std::fmt;

use crate::codec::{DecodeError, Reader, put_bool, put_bytes, put_str, put_varint};
use crate::device::{self, DeviceKey};
use crate::ids::FolderId;

const MESSAGE: &str = "seedwell-chat/v1/message";

/// Longest text message, bytes.
pub const MAX_TEXT: usize = 16 * 1024;
/// Longest JSON message (and body of a kind this version does not know), bytes.
pub const MAX_JSON: usize = 64 * 1024;
/// Longest agent label, in characters.
pub const MAX_AGENT: usize = 64;
/// Longest topic title, in characters.
pub const MAX_TITLE: usize = 200;
/// Longest fields of one message on the wire.
pub const MAX_FIELDS: usize = MAX_JSON + 1024;
/// Ranges one author's summary may have.
pub const MAX_RANGES: usize = 4096;
/// Authors in one summary.
pub const MAX_AUTHORS: usize = 1024;

/// What a message holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// Text for people (1).
    Text,
    /// JSON, for agents (2).
    Json,
    /// A new topic (3): the body is its title, the message's id the topic's.
    Topic,
    /// A kind of a later version (reactions, edits): kept and passed on, not shown.
    Unknown(u8),
}

impl MessageKind {
    /// Its byte.
    pub fn byte(self) -> u8 {
        match self {
            MessageKind::Text => 1,
            MessageKind::Json => 2,
            MessageKind::Topic => 3,
            MessageKind::Unknown(b) => b,
        }
    }

    /// From its byte.
    pub fn from_byte(b: u8) -> MessageKind {
        match b {
            1 => MessageKind::Text,
            2 => MessageKind::Json,
            3 => MessageKind::Topic,
            b => MessageKind::Unknown(b),
        }
    }

    /// The most body bytes it may have.
    pub fn max_body(self) -> usize {
        match self {
            MessageKind::Text => MAX_TEXT,
            MessageKind::Topic => MAX_TITLE * 4,
            _ => MAX_JSON,
        }
    }
}

/// Why a message cannot be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatError {
    /// The body is longer than its kind allows.
    TooLong(usize),
    /// A text or JSON body that is not UTF-8.
    NotText,
    /// An agent label that is empty or longer than [`MAX_AGENT`] characters.
    Agent,
    /// A topic title that is empty or longer than [`MAX_TITLE`] characters.
    Title,
    /// A private message's recipient key gives no shared secret (a bad key).
    Key,
}

impl fmt::Display for ChatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatError::TooLong(max) => write!(f, "the message is longer than {} KiB", max / 1024),
            ChatError::NotText => f.write_str("the message is not text"),
            ChatError::Agent => write!(f, "an agent name is 1 to {MAX_AGENT} characters"),
            ChatError::Title => write!(f, "a topic's title is 1 to {MAX_TITLE} characters"),
            ChatError::Key => f.write_str("the recipient's key for private messages cannot be used"),
        }
    }
}

impl std::error::Error for ChatError {}

/// One message of a channel, signed by its author's device.
#[derive(Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// Random; names the message (replies point at it).
    pub id: [u8; 16],
    /// The device that posted it (its public key) and signed it.
    pub author: [u8; 32],
    /// The author's count of its messages in this channel: 1, 2, …
    pub seq: u64,
    /// When it was posted, Unix milliseconds (the author's clock).
    pub at_ms: u64,
    /// The message it answers.
    pub reply_to: Option<[u8; 16]>,
    /// The topic it belongs to (a topic message's id); `None`: General.
    pub topic: Option<[u8; 16]>,
    /// The agent (control client) that posted it through the author's service, if one did.
    pub agent: Option<String>,
    /// What the body is.
    pub kind: MessageKind,
    /// The body.
    pub body: Vec<u8>,
    /// Fields a later version appended (kept, signed, passed on).
    pub extra: Vec<u8>,
    /// The author's signature.
    pub sig: [u8; 64],
}

impl fmt::Debug for ChatMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChatMessage({}, seq {}, {:?}, {} bytes)", crate::members::hex(&self.id), self.seq, self.kind, self.body.len())
    }
}

/// What a device makes a message from (the author's service fills in the rest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    /// Random id.
    pub id: [u8; 16],
    /// The author's next sequence number in the channel.
    pub seq: u64,
    /// Now, Unix milliseconds.
    pub at_ms: u64,
    /// The message it answers.
    pub reply_to: Option<[u8; 16]>,
    /// Its topic (`None`: General).
    pub topic: Option<[u8; 16]>,
    /// The agent that posts it (the control client's name), if any.
    pub agent: Option<String>,
    /// Text or JSON.
    pub kind: MessageKind,
    /// The body.
    pub body: Vec<u8>,
}

pub(crate) fn check_body(kind: MessageKind, body: &[u8]) -> Result<(), ChatError> {
    if body.len() > kind.max_body() {
        return Err(ChatError::TooLong(kind.max_body()));
    }
    if matches!(kind, MessageKind::Text | MessageKind::Json | MessageKind::Topic) {
        let Ok(text) = std::str::from_utf8(body) else { return Err(ChatError::NotText) };
        if kind == MessageKind::Topic && (text.trim().is_empty() || text.chars().count() > MAX_TITLE) {
            return Err(ChatError::Title);
        }
    }
    Ok(())
}

pub(crate) fn check_agent(agent: Option<&str>) -> Result<(), ChatError> {
    match agent {
        Some(a) if a.is_empty() || a.chars().count() > MAX_AGENT || a.len() > MAX_AGENT * 4 => Err(ChatError::Agent),
        _ => Ok(()),
    }
}

impl ChatMessage {
    /// Signs a draft as `key`'s device for the channel `channel`.
    pub fn sign(key: &DeviceKey, channel: &FolderId, d: Draft) -> Result<ChatMessage, ChatError> {
        check_body(d.kind, &d.body)?;
        check_agent(d.agent.as_deref())?;
        let mut m = ChatMessage {
            id: d.id,
            author: *key.public_key(),
            seq: d.seq,
            at_ms: d.at_ms,
            reply_to: d.reply_to,
            topic: d.topic,
            agent: d.agent,
            kind: d.kind,
            body: d.body,
            extra: Vec::new(),
            sig: [0; 64],
        };
        m.sig = key.sign(&m.signed_bytes(channel));
        Ok(m)
    }

    fn fields(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 128);
        out.extend_from_slice(&self.id);
        out.extend_from_slice(&self.author);
        put_varint(&mut out, self.seq);
        put_varint(&mut out, self.at_ms);
        match &self.reply_to {
            Some(r) => {
                put_bool(&mut out, true);
                out.extend_from_slice(r);
            }
            None => put_bool(&mut out, false),
        }
        match &self.topic {
            Some(t) => {
                put_bool(&mut out, true);
                out.extend_from_slice(t);
            }
            None => put_bool(&mut out, false),
        }
        put_str(&mut out, self.agent.as_deref().unwrap_or(""));
        out.push(self.kind.byte());
        put_bytes(&mut out, &self.body);
        out.extend_from_slice(&self.extra);
        out
    }

    fn signed_bytes(&self, channel: &FolderId) -> Vec<u8> {
        let mut out = MESSAGE.as_bytes().to_vec();
        out.extend_from_slice(&channel.0);
        put_bytes(&mut out, &self.fields());
        out
    }

    /// The author signed it for this channel.
    pub fn verify(&self, channel: &FolderId) -> bool {
        device::verify(&self.author, &self.signed_bytes(channel), &self.sig)
    }

    /// The wire form.
    pub fn put(&self, out: &mut Vec<u8>) {
        put_bytes(out, &self.fields());
        out.extend_from_slice(&self.sig);
    }

    /// The wire form, on its own.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 200);
        self.put(&mut out);
        out
    }

    /// Reads one message (the signature is checked by [`ChatMessage::verify`]). A text or JSON body that is too
    /// long or not UTF-8 is [`DecodeError::Invalid`], so a list skips the message.
    pub fn read(r: &mut Reader<'_>) -> Result<ChatMessage, DecodeError> {
        let fields = r.bytes(MAX_FIELDS, "message")?;
        let sig = r.array("message signature")?;
        let f = &mut Reader::new(fields);
        let id = f.array("message id")?;
        let author = f.array("author")?;
        let seq = f.varint("message seq")?;
        let at_ms = f.varint("message time")?;
        let reply_to = if f.bool("has reply")? { Some(f.array("reply to")?) } else { None };
        let topic = if f.bool("has topic")? { Some(f.array("topic")?) } else { None };
        let agent = f.str(MAX_AGENT * 4, "agent")?;
        let agent = (!agent.is_empty()).then_some(agent);
        if check_agent(agent.as_deref()).is_err() {
            return Err(DecodeError::Invalid("agent"));
        }
        let kind = MessageKind::from_byte(f.u8("message kind")?);
        let body = f.bytes(MAX_JSON, "message body")?.to_vec();
        if check_body(kind, &body).is_err() {
            return Err(DecodeError::Invalid("message body"));
        }
        let extra = f.take(f.remaining(), "message extra")?.to_vec();
        Ok(ChatMessage { id, author, seq, at_ms, reply_to, topic, agent, kind, body, extra, sig })
    }

    /// Reads a message from its own bytes.
    pub fn from_bytes(b: &[u8]) -> Result<ChatMessage, DecodeError> {
        ChatMessage::read(&mut Reader::new(b))
    }

    /// The body as text (text, JSON and a topic's title).
    pub fn text(&self) -> Option<&str> {
        match self.kind {
            MessageKind::Text | MessageKind::Json | MessageKind::Topic => std::str::from_utf8(&self.body).ok(),
            MessageKind::Unknown(_) => None,
        }
    }
}

/// Which messages of one author a device holds: sequence ranges `[start, start + len)`, sorted, not touching.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthorHave {
    /// The author's public key.
    pub author: [u8; 32],
    /// `(start, len)`.
    pub ranges: Vec<(u64, u64)>,
}

impl AuthorHave {
    /// The ranges of these sequence numbers (any order, repeats allowed); at most [`MAX_RANGES`], the newest kept.
    pub fn of(author: [u8; 32], seqs: impl IntoIterator<Item = u64>) -> AuthorHave {
        let mut s: Vec<u64> = seqs.into_iter().collect();
        s.sort_unstable();
        s.dedup();
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for q in s {
            match ranges.last_mut() {
                Some((start, len)) if *start + *len == q => *len += 1,
                _ => ranges.push((q, 1)),
            }
        }
        if ranges.len() > MAX_RANGES {
            ranges.drain(..ranges.len() - MAX_RANGES);
        }
        AuthorHave { author, ranges }
    }

    /// Holds sequence number `seq`.
    pub fn has(&self, seq: u64) -> bool {
        let i = self.ranges.partition_point(|(start, _)| *start <= seq);
        i > 0 && seq < self.ranges[i - 1].0.saturating_add(self.ranges[i - 1].1)
    }

    /// The highest sequence number held, if any.
    pub fn highest(&self) -> Option<u64> {
        self.ranges.last().map(|(s, l)| s + l - 1)
    }

    /// The wire form.
    pub fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.author);
        crate::codec::put_len(out, self.ranges.len());
        for (s, l) in &self.ranges {
            put_varint(out, *s);
            put_varint(out, *l);
        }
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<AuthorHave, DecodeError> {
        let author = r.array("author")?;
        let ranges = r.list(MAX_RANGES, 2, "ranges", |r| {
            let start = r.varint("range start")?;
            let len = r.varint("range length")?;
            if len == 0 || start.checked_add(len).is_none() {
                return Err(DecodeError::Invalid("range"));
            }
            Ok((start, len))
        })?;
        Ok(AuthorHave { author, ranges })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(kind: MessageKind, body: &[u8]) -> Draft {
        Draft { id: [1; 16], seq: 1, at_ms: 1_800_000_000_000, reply_to: Some([2; 16]), topic: Some([7; 16]), agent: Some("claude".into()), kind, body: body.to_vec() }
    }

    #[test]
    fn messages_roundtrip_and_the_agent_label_is_signed() {
        let key = DeviceKey::from_seed(&[3; 32]);
        let ch = FolderId([4; 32]);
        let m = ChatMessage::sign(&key, &ch, draft(MessageKind::Text, "hei på deg".as_bytes())).unwrap();
        let back = ChatMessage::from_bytes(&m.to_bytes()).unwrap();
        assert_eq!(back, m);
        assert!(back.verify(&ch));
        assert!(!back.verify(&FolderId([5; 32])), "signed for one channel");
        assert_eq!(back.text(), Some("hei på deg"));
        for change in [
            |m: &mut ChatMessage| m.agent = None,
            |m: &mut ChatMessage| m.agent = Some("someone else".into()),
            |m: &mut ChatMessage| m.reply_to = None,
            |m: &mut ChatMessage| m.topic = None,
            |m: &mut ChatMessage| m.seq = 2,
            |m: &mut ChatMessage| m.body.push(b'!'),
        ] {
            let mut x = m.clone();
            change(&mut x);
            assert!(!x.verify(&ch), "{x:?}");
        }
    }

    #[test]
    fn sizes_and_text_are_checked() {
        let key = DeviceKey::from_seed(&[3; 32]);
        let ch = FolderId([4; 32]);
        assert_eq!(ChatMessage::sign(&key, &ch, draft(MessageKind::Text, &vec![b'a'; MAX_TEXT + 1])), Err(ChatError::TooLong(MAX_TEXT)));
        assert!(ChatMessage::sign(&key, &ch, draft(MessageKind::Json, &vec![b'1'; MAX_TEXT + 1])).is_ok());
        assert_eq!(ChatMessage::sign(&key, &ch, draft(MessageKind::Text, &[0xff])), Err(ChatError::NotText));
        let mut d = draft(MessageKind::Text, b"x");
        d.agent = Some("a".repeat(MAX_AGENT + 1));
        assert_eq!(ChatMessage::sign(&key, &ch, d), Err(ChatError::Agent));
        assert_eq!(ChatMessage::sign(&key, &ch, draft(MessageKind::Topic, b"  ")), Err(ChatError::Title));
        assert_eq!(ChatMessage::sign(&key, &ch, draft(MessageKind::Topic, "x".repeat(MAX_TITLE + 1).as_bytes())), Err(ChatError::Title));
        let t = ChatMessage::sign(&key, &ch, draft(MessageKind::Topic, "Middag".as_bytes())).unwrap();
        assert_eq!(ChatMessage::from_bytes(&t.to_bytes()).unwrap().text(), Some("Middag"));
        // A kind of a later version may hold any bytes, and reads back as it came.
        let odd = ChatMessage::sign(&key, &ch, draft(MessageKind::Unknown(40), &[0xff, 0])).unwrap();
        let back = ChatMessage::from_bytes(&odd.to_bytes()).unwrap();
        assert_eq!((back.kind, back.text(), back.verify(&ch)), (MessageKind::Unknown(40), None, true));
    }

    #[test]
    fn summaries_are_ranges() {
        let h = AuthorHave::of([1; 32], [5, 1, 2, 3, 3, 9, 10]);
        assert_eq!(h.ranges, vec![(1, 3), (5, 1), (9, 2)]);
        assert!(h.has(2) && h.has(10) && !h.has(4) && !h.has(11) && !h.has(0));
        assert_eq!(h.highest(), Some(10));
        let mut out = Vec::new();
        h.put(&mut out);
        assert_eq!(AuthorHave::read(&mut Reader::new(&out)), Ok(h));
        assert_eq!(AuthorHave::of([1; 32], 1..=(2 * MAX_RANGES as u64)).ranges, vec![(1, 2 * MAX_RANGES as u64)]);
        let many = AuthorHave::of([1; 32], (0..(MAX_RANGES as u64 + 5)).map(|i| i * 2));
        assert_eq!((many.ranges.len(), many.highest()), (MAX_RANGES, Some((MAX_RANGES as u64 + 4) * 2)), "the newest kept");
    }
}
