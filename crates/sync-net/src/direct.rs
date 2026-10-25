//! Private messages (SPEC §7G.7 *Direct messages*): between two devices that share a channel, carried over the
//! channels' sessions, end-to-end encrypted to the recipient ([`trs_sync_proto::direct`]).
//!
//! **Keys.** Each device's X25519 key comes from its device key; its public half goes out in a signed record
//! ([`DirectKey`]) when a channel starts over a session, with the records this device knows of the other members
//! of the channels it shares with that peer, and again when a channel's members change. A new record is passed on the
//! same way, for members only; one of a device not known as a member yet is kept too (up to [`MAX_KEPT_KEYS`]), as a
//! new member's key can arrive before the news that it was admitted.
//!
//! **Carrying.** A message goes to every connected member that is its recipient or shares a channel with it. A
//! member that is not the recipient keeps the sealed message (it cannot read it) and passes it on; the recipient
//! gets it from whoever has it when it connects ([`DirectHave`]: what it holds, as ranges per author). Only
//! messages between devices that both share a channel with the carrying device are taken. The author keeps its own
//! copy in the clear on its own disk. Capped like a channel ([`crate::chat::MAX_MESSAGES`], [`crate::chat::MAX_BYTES`]).
//!
//! On disk: `sync/direct.keys` (`"SWDK" ‖ format 1 ‖ records`) and `sync/direct.chat` (`"SWDM" ‖ format 1 ‖ n ×
//! (varint length ‖ received_ms varint ‖ has_body u8 [‖ body bytes] ‖ message)`; the body only in the author's
//! own copies).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use trs_sync_proto::chat::{AuthorHave, MessageKind};
use trs_sync_proto::codec::{Reader, put_bytes, put_varint};
use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::direct::{DirectBody, DirectDraft, DirectKey, DirectMessage, public_of, secret_of};
use trs_sync_proto::ids::DeviceId;
use trs_sync_proto::members::hex;
use trs_sync_proto::msg::{DirectHave, DirectKeys, DirectMessages, Message};

use crate::chat::{AgentAccess, ChatEvent, FUTURE_SLACK_MS, Key, MAX_BYTES, MAX_MESSAGES, MessageInfo};
use crate::config::DirectEntry;
use crate::service::{SyncError, SyncHandle, invalid, random_id};
use crate::shared::{Ctx, PeerHandle, lock};

const KEYS_MAGIC: &[u8] = b"SWDK";
const MSGS_MAGIC: &[u8] = b"SWDM";
const FORMAT: u64 = 1;

/// Key records kept at most; past it, only members' are taken.
pub const MAX_KEPT_KEYS: usize = 4096;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// One private conversation, for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectStatus {
    /// The other device.
    pub device: DeviceId,
    /// Its name (empty when never met).
    pub name: String,
    /// A session with it is open now.
    pub online: bool,
    /// Its messages since the last read.
    pub unread: u32,
    /// When the newest message was written.
    pub last_message_s: Option<u64>,
}

struct Stored {
    msg: DirectMessage,
    received_ms: u64,
    /// What it says: this device's own messages (kept in the clear) and those to it (opened).
    body: Option<DirectBody>,
}

fn shown_at(m: &DirectMessage, received_ms: u64) -> u64 {
    if m.at_ms > received_ms.saturating_add(FUTURE_SLACK_MS) { received_ms } else { m.at_ms }
}

/// Every private message this device keeps (its own, those to it, and those it carries).
struct Store {
    path: PathBuf,
    msgs: BTreeMap<Key, Stored>,
    ids: HashMap<[u8; 16], Key>,
    seqs: HashMap<[u8; 32], BTreeSet<u64>>,
    bytes: usize,
}

fn record(s: &Stored) -> Vec<u8> {
    let mut rec = Vec::with_capacity(s.msg.sealed.len() + 300);
    put_varint(&mut rec, s.received_ms);
    // Only the author's own copy keeps the text (see `own_copy`); one to this device is opened again when read.
    match s.body.as_ref() {
        Some(b) => {
            rec.push(1);
            put_bytes(&mut rec, &b.to_bytes());
        }
        None => rec.push(0),
    }
    s.msg.put(&mut rec);
    let mut out = Vec::with_capacity(rec.len() + 4);
    put_bytes(&mut out, &rec);
    out
}

impl Store {
    fn open(path: PathBuf, me: &[u8; 32], secret: &[u8; 32]) -> io::Result<Store> {
        let mut s = Store { path, msgs: BTreeMap::new(), ids: HashMap::new(), seqs: HashMap::new(), bytes: 0 };
        let bytes = match std::fs::read(&s.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                s.rewrite(me)?;
                return Ok(s);
            }
            Err(e) => return Err(e),
        };
        let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {why}", s.path.display()));
        let body = bytes.strip_prefix(MSGS_MAGIC).ok_or_else(|| bad("not a private messages file"))?;
        let mut r = Reader::new(body);
        match r.varint("format") {
            Ok(FORMAT) => {}
            Ok(n) => return Err(bad(&format!("written by a newer Seedwell (format {n}): update Seedwell"))),
            Err(e) => return Err(bad(&e.to_string())),
        }
        let mut damaged = false;
        while r.remaining() > 0 {
            let Ok(rec) = r.bytes(usize::MAX >> 1, "record") else {
                damaged = true;
                break;
            };
            let rr = &mut Reader::new(rec);
            let parsed = (|| {
                let received_ms = rr.varint("received").ok()?;
                let plain = if rr.bool("has body").ok()? { DirectBody::from_bytes(rr.bytes(usize::MAX >> 1, "body").ok()?).ok() } else { None };
                let msg = DirectMessage::read(rr).ok()?;
                Some((received_ms, plain, msg))
            })();
            match parsed {
                Some((received_ms, plain, msg)) => {
                    let body = if msg.author == *me { plain } else if msg.recipient == *me { msg.open(secret) } else { None };
                    s.put(Stored { msg, received_ms, body });
                }
                None => damaged = true,
            }
        }
        if s.prune() || damaged {
            s.rewrite(me)?;
        }
        Ok(s)
    }

    fn put(&mut self, s: Stored) -> bool {
        if self.ids.contains_key(&s.msg.id) || self.seqs.get(&s.msg.author).is_some_and(|q| q.contains(&s.msg.seq)) {
            return false;
        }
        let key = (shown_at(&s.msg, s.received_ms), s.msg.seq, s.msg.id);
        self.ids.insert(s.msg.id, key);
        self.seqs.entry(s.msg.author).or_default().insert(s.msg.seq);
        self.bytes += s.msg.sealed.len() + 300;
        self.msgs.insert(key, s);
        true
    }

    fn prune(&mut self) -> bool {
        if self.msgs.len() <= MAX_MESSAGES && self.bytes <= MAX_BYTES {
            return false;
        }
        while self.msgs.len() > MAX_MESSAGES * 9 / 10 || self.bytes > MAX_BYTES / 10 * 9 {
            let Some((_, old)) = self.msgs.pop_first() else { break };
            self.ids.remove(&old.msg.id);
            if let Some(q) = self.seqs.get_mut(&old.msg.author) {
                q.remove(&old.msg.seq);
            }
            self.bytes = self.bytes.saturating_sub(old.msg.sealed.len() + 300);
        }
        true
    }

    fn rewrite(&self, me: &[u8; 32]) -> io::Result<()> {
        let mut out = MSGS_MAGIC.to_vec();
        put_varint(&mut out, FORMAT);
        for s in self.msgs.values() {
            out.extend_from_slice(&record(&own_copy(s, me)));
        }
        let tmp = self.path.with_extension("chat.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &self.path)
    }

    fn insert(&mut self, s: Stored, me: &[u8; 32]) -> io::Result<bool> {
        let rec = record(&own_copy(&s, me));
        if !self.put(s) {
            return Ok(false);
        }
        if self.prune() {
            self.rewrite(me)?;
        } else {
            std::fs::OpenOptions::new().append(true).open(&self.path)?.write_all(&rec)?;
        }
        Ok(true)
    }

    /// What this device holds of the messages to it.
    fn have_for(&self, me: &[u8; 32]) -> Vec<AuthorHave> {
        let mut by: HashMap<[u8; 32], Vec<u64>> = HashMap::new();
        for s in self.msgs.values().filter(|s| s.msg.recipient == *me) {
            by.entry(s.msg.author).or_default().push(s.msg.seq);
        }
        by.into_iter().map(|(a, q)| AuthorHave::of(a, q)).collect()
    }

    fn highest(&self, author: &[u8; 32]) -> u64 {
        self.seqs.get(author).and_then(|q| q.last().copied()).unwrap_or(0)
    }
}

/// Which body a stored record keeps: only the author's own copy is written in the clear.
fn own_copy(s: &Stored, me: &[u8; 32]) -> Stored {
    Stored { msg: s.msg.clone(), received_ms: s.received_ms, body: s.body.clone().filter(|_| s.msg.author == *me) }
}

/// This device's settings for private messages.
#[derive(Clone, Debug, Default)]
pub(crate) struct Settings {
    pub(crate) agents: AgentAccess,
    pub(crate) seq: u64,
    pub(crate) read: HashMap<[u8; 32], Key>,
}

fn settings_of(e: &DirectEntry) -> Settings {
    let read = e
        .read
        .iter()
        .filter_map(|t| {
            let mut p = t.split(':');
            let (dev, at, seq, mid) = (p.next()?, p.next()?, p.next()?, p.next()?);
            let dev = crate::service::public_key_from_hex(dev)?;
            Some((dev, (at.parse().ok()?, seq.parse().ok()?, trs_sync_proto::members::invite_id_from_hex(mid)?)))
        })
        .collect();
    Settings { agents: AgentAccess::parse(&e.agents).unwrap_or_default(), seq: e.seq, read }
}

/// Private messages on this device.
pub struct Direct {
    secret: [u8; 32],
    me: [u8; 32],
    mine: DirectKey,
    keys: Mutex<BTreeMap<[u8; 32], DirectKey>>,
    keys_path: PathBuf,
    store: Mutex<Store>,
    pub(crate) settings: Mutex<Settings>,
}

impl Direct {
    /// Opens this device's private messages in `dir`.
    pub(crate) fn open(dir: &std::path::Path, key: &DeviceKey, entry: &DirectEntry) -> io::Result<Direct> {
        let secret = secret_of(key);
        let me = *key.public_key();
        let keys_path = dir.join("direct.keys");
        let mut keys = BTreeMap::new();
        if let Ok(b) = std::fs::read(&keys_path)
            && let Some(rest) = b.strip_prefix(KEYS_MAGIC)
        {
            let r = &mut Reader::new(rest);
            if r.varint("format").ok() == Some(FORMAT) {
                while r.remaining() > 0 {
                    let Ok(k) = DirectKey::read(r) else { break };
                    if k.verify() {
                        keys.insert(k.device, k);
                    }
                }
            }
        }
        let store = Store::open(dir.join("direct.chat"), &me, &secret)?;
        Ok(Direct { secret, me, mine: DirectKey::of(key, now_ms()), keys: Mutex::new(keys), keys_path, store: Mutex::new(store), settings: Mutex::new(settings_of(entry)) })
    }

    fn save_keys(&self, keys: &BTreeMap<[u8; 32], DirectKey>) {
        let mut out = KEYS_MAGIC.to_vec();
        put_varint(&mut out, FORMAT);
        for k in keys.values() {
            k.put(&mut out);
        }
        let tmp = self.keys_path.with_extension("keys.tmp");
        let _ = std::fs::write(&tmp, out).and_then(|()| std::fs::rename(&tmp, &self.keys_path));
    }

    /// Keeps a key record (signed, of a device we share a channel with); true when it is new or newer.
    fn learn(&self, k: DirectKey) -> bool {
        if k.device == self.me || !k.verify() {
            return false;
        }
        let mut keys = lock(&self.keys);
        if keys.get(&k.device).is_some_and(|old| old.at_ms >= k.at_ms) {
            return false;
        }
        keys.insert(k.device, k);
        self.save_keys(&keys);
        true
    }
}

/// A device that is an active member of some channel this device is in.
fn in_a_channel(ctx: &Ctx, pk: &[u8; 32]) -> bool {
    ctx.registry.channels().iter().any(|c| c.role_of(pk).is_some())
}

/// `peer` may carry messages for `recipient`: it is the recipient, or shares a channel with it here.
fn carries_for(ctx: &Ctx, peer: &PeerHandle, recipient: &[u8; 32]) -> bool {
    peer.device_pk == *recipient || ctx.registry.channels().iter().any(|c| peer.access(&c.id).is_some() && c.role_of(recipient).is_some())
}

/// The key records to give `peer`: ours, and those of the other members of the channels we share with it.
fn keys_for(ctx: &Ctx, peer: &PeerHandle) -> Vec<DirectKey> {
    let shared: Vec<_> = ctx.registry.channels().into_iter().filter(|c| peer.access(&c.id).is_some()).collect();
    let mut out = vec![ctx.direct.mine];
    for k in lock(&ctx.direct.keys).values() {
        if k.device != peer.device_pk && shared.iter().any(|c| c.role_of(&k.device).is_some()) {
            out.push(*k);
        }
    }
    out
}

/// A channel started over a session: our keys, and what we hold of the messages to us.
pub(crate) async fn start(ctx: &Arc<Ctx>, peer: &Arc<PeerHandle>) {
    let have = lock(&ctx.direct.store).have_for(&ctx.direct.me);
    let _ = peer.control.send(Message::DirectKeys(DirectKeys { records: keys_for(ctx, peer) })).await;
    let _ = peer.control.send(Message::DirectHave(DirectHave { authors: have })).await;
}

/// A channel's membership changed: its connected members get the key records again.
pub(crate) async fn members_changed(ctx: &Arc<Ctx>, channel: &trs_sync_proto::ids::FolderId) {
    for p in ctx.registry.peers() {
        if p.chat() && p.access(channel).is_some() {
            let _ = p.control.send(Message::DirectKeys(DirectKeys { records: keys_for(ctx, &p) })).await;
        }
    }
}

/// Key records from a peer: the new ones are kept, and passed on to the members of the channels their devices are in.
pub(crate) async fn on_keys(ctx: &Arc<Ctx>, from: &PeerHandle, records: Vec<DirectKey>) {
    let fresh: Vec<DirectKey> =
        records.into_iter().filter(|k| (in_a_channel(ctx, &k.device) || lock(&ctx.direct.keys).len() < MAX_KEPT_KEYS) && ctx.direct.learn(*k)).collect();
    if fresh.is_empty() {
        return;
    }
    for p in ctx.registry.peers() {
        if p.device == from.device || !p.chat() {
            continue;
        }
        let pass: Vec<DirectKey> = fresh.iter().filter(|k| k.device != p.device_pk && carries_for(ctx, &p, &k.device)).copied().collect();
        if !pass.is_empty() {
            let _ = p.control.send(Message::DirectKeys(DirectKeys { records: pass })).await;
        }
    }
}

/// A peer said what it holds of the messages to it: send what it lacks.
pub(crate) async fn on_have(ctx: &Arc<Ctx>, peer: &Arc<PeerHandle>, have: DirectHave) {
    let theirs: HashMap<&[u8; 32], &AuthorHave> = have.authors.iter().map(|a| (&a.author, a)).collect();
    let me = ctx.direct.me;
    if let Some(high) = theirs.get(&me).and_then(|a| a.highest()) {
        let mut s = lock(&ctx.direct.settings);
        s.seq = s.seq.max(high);
    }
    let missing: Vec<DirectMessage> = lock(&ctx.direct.store)
        .msgs
        .values()
        .filter(|s| s.msg.recipient == peer.device_pk && !theirs.get(&s.msg.author).is_some_and(|a| a.has(s.msg.seq)))
        .map(|s| s.msg.clone())
        .collect();
    for part in missing.chunks(500) {
        if peer.control.send(Message::DirectMessages(DirectMessages { messages: part.to_vec() })).await.is_err() {
            return;
        }
    }
}

/// Messages from a peer: kept when signed and between devices that share a channel with this one; those to this
/// device are opened (one that does not open is dropped); the new ones are passed on and told about.
pub(crate) async fn on_messages(ctx: &Arc<Ctx>, from: DeviceId, msgs: Vec<DirectMessage>) {
    let d = &ctx.direct;
    let mut new = Vec::new();
    for m in msgs {
        if !m.verify() || m.author == d.me || !in_a_channel(ctx, &m.author) {
            continue;
        }
        let body = if m.recipient == d.me {
            match m.open(&d.secret) {
                Some(b) => Some(b),
                None => continue,
            }
        } else if in_a_channel(ctx, &m.recipient) {
            None
        } else {
            continue;
        };
        let stored = Stored { msg: m.clone(), received_ms: now_ms(), body };
        match lock(&d.store).insert(stored, &d.me) {
            Ok(true) => new.push(m),
            Ok(false) => {}
            Err(e) => eprintln!("seedwell chat: private messages could not be saved: {e}"),
        }
    }
    pass_on(ctx, &new, Some(from)).await;
}

/// Tells watchers about new messages to or from this device, and passes them to the connected devices that carry for
/// their recipients (but not back to where they came from).
async fn pass_on(ctx: &Arc<Ctx>, new: &[DirectMessage], from: Option<DeviceId>) {
    if new.is_empty() {
        return;
    }
    let names: HashMap<DeviceId, String> = ctx.registry.peers().iter().map(|p| (p.device, p.name.clone())).collect();
    let agents = lock(&ctx.direct.settings).agents;
    for m in new {
        let info = {
            let store = lock(&ctx.direct.store);
            store.ids.get(&m.id).and_then(|k| store.msgs.get(k)).and_then(|s| info(ctx, s, &names))
        };
        if let Some(i) = info {
            let _ = ctx.chat_events.send(ChatEvent { message: i, agents });
        }
    }
    for p in ctx.registry.peers() {
        if Some(p.device) == from || !p.chat() {
            continue;
        }
        let pass: Vec<DirectMessage> = new.iter().filter(|m| m.author != p.device_pk && carries_for(ctx, &p, &m.recipient)).cloned().collect();
        if !pass.is_empty() {
            let _ = p.control.send(Message::DirectMessages(DirectMessages { messages: pass })).await;
        }
    }
}

/// A stored message as the UI sees it: only this device's own and those to it.
fn info(ctx: &Ctx, s: &Stored, names: &HashMap<DeviceId, String>) -> Option<MessageInfo> {
    let b = s.body.as_ref()?;
    let me = ctx.direct.me;
    let m = &s.msg;
    let author = DeviceId::of_public_key(&m.author);
    let other = if m.author == me { m.recipient } else { m.author };
    let at_ms = shown_at(m, s.received_ms);
    let (kind, body) = match b.kind {
        MessageKind::Text => ("text", b.text().unwrap_or_default().to_string()),
        MessageKind::Json => ("json", b.text().unwrap_or_default().to_string()),
        _ => ("other", String::new()),
    };
    let mine = m.author == me;
    Some(MessageInfo {
        id: hex(&m.id),
        channel_id: String::new(),
        channel_name: String::new(),
        author_device: author.to_string(),
        author_name: if mine { lock(&ctx.device_name).clone() } else { names.get(&author).cloned().unwrap_or_default() },
        me: mine,
        agent: b.agent.clone(),
        at: at_ms / 1000,
        future: at_ms != m.at_ms,
        reply_to: b.reply_to.map(|r| hex(&r)),
        topic: None,
        topic_title: None,
        direct: true,
        peer_device: Some(DeviceId::of_public_key(&other).to_string()),
        kind: kind.into(),
        body,
        trust: if mine { "this-device" } else { "untrusted-peer" }.into(),
    })
}

impl SyncHandle {
    /// The public key of a device that is an active member of a channel this device is in.
    fn channel_member(&self, device: &DeviceId) -> Option<[u8; 32]> {
        self.inner.ctx.registry.channels().iter().find_map(|c| {
            let m = c.membership.read().unwrap_or_else(|e| e.into_inner());
            m.members().map(|(pk, _)| *pk).find(|pk| DeviceId::of_public_key(pk) == *device)
        })
    }

    fn device_names(&self) -> HashMap<DeviceId, String> {
        let mut out: HashMap<DeviceId, String> =
            lock(&self.inner.cfg).devices.iter().filter_map(|d| Some((d.id.parse().ok()?, d.name.clone()))).collect();
        for p in self.inner.ctx.registry.peers() {
            out.insert(p.device, p.name.clone());
        }
        out
    }

    /// Every device this one has private messages with, newest first. Never waits for anything slow.
    pub fn chat_directs(&self) -> Vec<DirectStatus> {
        let d = &self.inner.ctx.direct;
        let names = self.device_names();
        let reads = lock(&d.settings).read.clone();
        let mut by: BTreeMap<[u8; 32], (u32, u64)> = BTreeMap::new();
        for (k, s) in lock(&d.store).msgs.iter() {
            if s.body.is_none() {
                continue;
            }
            let other = if s.msg.author == d.me { s.msg.recipient } else { s.msg.author };
            let e = by.entry(other).or_default();
            if s.msg.author != d.me && *k > reads.get(&other).copied().unwrap_or_default() {
                e.0 += 1;
            }
            e.1 = e.1.max(k.0);
        }
        let mut out: Vec<DirectStatus> = by
            .into_iter()
            .map(|(pk, (unread, last))| {
                let device = DeviceId::of_public_key(&pk);
                DirectStatus {
                    device,
                    name: names.get(&device).cloned().unwrap_or_default(),
                    online: self.inner.ctx.registry.peer(&device).is_some(),
                    unread,
                    last_message_s: Some(last / 1000),
                }
            })
            .collect();
        out.sort_by(|a, b| b.last_message_s.cmp(&a.last_message_s).then_with(|| a.name.cmp(&b.name)));
        out
    }

    /// The private messages with `device`, oldest first: the `limit` newest before `before`, or the newest.
    pub fn chat_direct_messages(&self, device: &DeviceId, before: Option<&str>, limit: usize) -> Result<Vec<MessageInfo>, SyncError> {
        let d = &self.inner.ctx.direct;
        let names = self.device_names();
        let store = lock(&d.store);
        let end = match before {
            Some(b) => Some(
                trs_sync_proto::members::invite_id_from_hex(b).and_then(|id| store.ids.get(&id).copied()).ok_or_else(|| invalid("no such message"))?,
            ),
            None => None,
        };
        let with = |s: &Stored| {
            let other = if s.msg.author == d.me { s.msg.recipient } else { s.msg.author };
            s.body.is_some() && DeviceId::of_public_key(&other) == *device
        };
        let mut page: Vec<MessageInfo> = store
            .msgs
            .iter()
            .rev()
            .filter(|(k, s)| end.is_none_or(|e| **k < e) && with(s))
            .take(limit)
            .filter_map(|(_, s)| info(&self.inner.ctx, s, &names))
            .collect();
        page.reverse();
        Ok(page)
    }

    /// Writes a private message to `device` (it must share a channel with this one, and its key must be known);
    /// `agent` as for channels.
    pub async fn chat_send_direct(&self, device: &DeviceId, kind: MessageKind, body: &str, reply_to: Option<&str>, agent: Option<&str>) -> Result<MessageInfo, SyncError> {
        let ctx = &self.inner.ctx;
        let d = &ctx.direct;
        if *device == ctx.me {
            return Err(invalid("a private message goes to another device"));
        }
        let recipient = self.channel_member(device).ok_or_else(|| invalid("this Mac shares no channel with that device, so it cannot write to it privately"))?;
        let key = lock(&d.keys).get(&recipient).copied().ok_or_else(|| {
            invalid("this Mac does not have that device's key for private messages yet; it comes when that device, or another member of a channel you share, is online")
        })?;
        if kind == MessageKind::Json && serde_json::from_str::<serde_json::Value>(body).is_err() {
            return Err(invalid("the message is not JSON"));
        }
        let reply_to = match reply_to {
            Some(r) => Some(trs_sync_proto::members::invite_id_from_hex(r).filter(|r| lock(&d.store).ids.contains_key(r)).ok_or_else(|| invalid("no such message to reply to"))?),
            None => None,
        };
        let mut eph = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut eph).map_err(|_| invalid("no random numbers"))?;
        let plain = DirectBody { reply_to, agent: agent.map(str::to_string), kind, body: body.as_bytes().to_vec() };
        let msg = {
            let mut s = lock(&d.settings);
            let seq = s.seq.max(lock(&d.store).highest(&d.me)) + 1;
            let draft = DirectDraft { id: random_id(), seq, at_ms: now_ms(), body: plain.clone(), ephemeral_secret: eph };
            let msg = DirectMessage::seal(&ctx.key, &recipient, &key.x25519, draft).map_err(|e| invalid(e.to_string()))?;
            s.seq = seq;
            msg
        };
        let stored = Stored { msg: msg.clone(), received_ms: now_ms(), body: Some(plain) };
        if !lock(&d.store).insert(stored, &d.me)? {
            return Err(invalid("the message could not be kept"));
        }
        lock(&self.inner.cfg).direct.seq = msg.seq;
        self.save_config()?;
        let names = self.device_names();
        let out = {
            let store = lock(&d.store);
            store.ids.get(&msg.id).and_then(|k| store.msgs.get(k)).and_then(|s| info(ctx, s, &names))
        };
        pass_on(ctx, std::slice::from_ref(&msg), None).await;
        out.ok_or(SyncError::NotFound)
    }

    /// Marks every private message from `device` read.
    pub fn chat_mark_direct_read(&self, device: &DeviceId) -> Result<(), SyncError> {
        let d = &self.inner.ctx.direct;
        let last = lock(&d.store)
            .msgs
            .iter()
            .rev()
            .find(|(_, s)| s.body.is_some() && s.msg.author != d.me && DeviceId::of_public_key(&s.msg.author) == *device)
            .map(|(k, s)| (*k, s.msg.author));
        let Some((last, pk)) = last else { return Ok(()) };
        let reads = {
            let mut s = lock(&d.settings);
            if s.read.get(&pk) == Some(&last) {
                return Ok(());
            }
            s.read.insert(pk, last);
            s.read.clone()
        };
        let mut v: Vec<String> = reads.iter().map(|(p, k)| format!("{}:{}:{}:{}", hex(p), k.0, k.1, hex(&k.2))).collect();
        v.sort();
        lock(&self.inner.cfg).direct.read = v;
        self.save_config()?;
        Ok(())
    }

    /// What agents on this Mac may do with private messages.
    pub fn chat_direct_agents(&self) -> AgentAccess {
        lock(&self.inner.ctx.direct.settings).agents
    }

    /// Sets what agents on this Mac may do with private messages.
    pub fn chat_set_direct_agents(&self, access: AgentAccess) -> Result<(), SyncError> {
        lock(&self.inner.ctx.direct.settings).agents = access;
        lock(&self.inner.cfg).direct.agents = access.name().into();
        self.save_config()?;
        Ok(())
    }

    /// **Tests only:** the X25519 public key this device publishes.
    #[doc(hidden)]
    pub fn test_direct_key(&self) -> [u8; 32] {
        public_of(&self.inner.ctx.direct.secret)
    }

    /// **Tests only:** this device has `device`'s key for private messages.
    #[doc(hidden)]
    pub fn test_direct_knows(&self, device: &DeviceId) -> bool {
        lock(&self.inner.ctx.direct.keys).keys().any(|pk| DeviceId::of_public_key(pk) == *device)
    }

    /// **Tests only:** how many private messages this device keeps (its own, to it, and carried).
    #[doc(hidden)]
    pub fn test_direct_held(&self) -> usize {
        lock(&self.inner.ctx.direct.store).msgs.len()
    }

    /// **Tests only:** the kept messages in wire form (to tamper with in a test).
    #[doc(hidden)]
    pub fn test_direct_wire(&self) -> Vec<Vec<u8>> {
        lock(&self.inner.ctx.direct.store).msgs.values().map(|s| s.msg.to_bytes()).collect()
    }

    /// **Tests only:** messages (wire form) arrive as if `from` passed them on.
    #[doc(hidden)]
    pub async fn test_direct_receive(&self, from: DeviceId, messages: &[Vec<u8>]) {
        let msgs = messages.iter().filter_map(|b| DirectMessage::from_bytes(b).ok()).collect();
        on_messages(&self.inner.ctx, from, msgs).await;
    }
}
