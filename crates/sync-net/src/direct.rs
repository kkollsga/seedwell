//! Private messages (SPEC §7G.7 *Direct messages*): between two devices, end-to-end encrypted to the recipient
//! ([`trs_sync_proto::direct`]), and only over a session between the two of them that no other device relays
//! ([`trs_sync_proto::msg::FEATURE_DIRECT`]).
//!
//! **Conversations.** Each conversation is a container of its own: the other device as this one knows it (its key,
//! names and last addresses, a `[[conversation]]` entry in `sync.toml`) and its messages (`sync/dm-<device>.chat`,
//! sealed with the conversation's own key, [`crate::at_rest`]). One starts when two devices that share a channel
//! write to each other; after that it stands on its own: while both keep it, a session may run for it alone
//! ([`trs_sync_proto::msg::DirectJoin`]), so leaving the channel does not end it. Deleting it deletes all three.
//!
//! **Delivery.** A message is written only while a direct session with its recipient is open, and goes only there:
//! no device carries another's messages or passes on another's key. Each side sends its own key record
//! ([`DirectKey`]) when such a session starts, and says what it holds of the other's messages ([`DirectHave`]), so a
//! message written as a session ended arrives with the next one. The author keeps its own copy (in the sealed file).
//! Capped like a channel ([`crate::chat::MAX_MESSAGES`], [`crate::chat::MAX_BYTES`]), per conversation.
//!
//! On disk, a conversation's messages: `"SWDM" ‖ format 2 ‖ n × (varint length ‖ sealed record)`, a record being
//! `received_ms varint ‖ has_body u8 [‖ body bytes] ‖ message` (the body only in the author's own copies). 0.1.8
//! kept every private message in one `sync/direct.chat` (format 1, in the clear, with those it carried for others)
//! and the keys it knew in `sync/direct.keys`: they are split into conversations when read, and deleted.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use trs_sync_proto::chat::{AuthorHave, MessageKind};
use trs_sync_proto::codec::{Reader, put_bytes, put_varint};
use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::direct::{DirectBody, DirectDraft, DirectKey, DirectMessage, public_of, secret_of};
use trs_sync_proto::ids::DeviceId;
use trs_sync_proto::members::hex;
use trs_sync_proto::msg::{DirectHave, DirectJoin, DirectKeys, DirectMessages, FEATURE_DIRECT, Message};

use crate::at_rest::{DIRECT_KEY, Sealer};
use crate::chat::{AgentAccess, ChatEvent, FUTURE_SLACK_MS, Key, MAX_BYTES, MAX_MESSAGES, MessageInfo};
use crate::config::{ConversationEntry, SyncConfig};
use crate::secrets::SecretStore;
use crate::service::{SyncError, SyncHandle, invalid, random_id};
use crate::shared::{Ctx, HubMsg, Names, PeerHandle, lock};

const MSGS_MAGIC: &[u8] = b"SWDM";
/// The messages file's format: its records sealed.
const MSGS_FORMAT: u64 = 2;

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
    /// Its user's name (empty: none given).
    pub user: String,
    /// A direct session with it is open now, so it can be written to.
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

/// The messages of one conversation (this device's own, and those to it).
struct Store {
    path: PathBuf,
    msgs: BTreeMap<Key, Stored>,
    ids: HashMap<[u8; 16], Key>,
    seqs: HashMap<[u8; 32], BTreeSet<u64>>,
    bytes: usize,
    seal: Sealer,
}

fn record(s: &Stored, seal: &Sealer) -> Vec<u8> {
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
    let sealed = seal.seal(&rec);
    let mut out = Vec::with_capacity(sealed.len() + 4);
    put_bytes(&mut out, &sealed);
    out
}

impl Store {
    fn open(path: PathBuf, me: &[u8; 32], secret: &[u8; 32], seal: Sealer) -> io::Result<Store> {
        let mut s = Store { path, msgs: BTreeMap::new(), ids: HashMap::new(), seqs: HashMap::new(), bytes: 0, seal };
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
        // Format 1 (before 0.1.9): the records in the clear, sealed now.
        let clear = match r.varint("format") {
            Ok(MSGS_FORMAT) => false,
            Ok(1) => true,
            Ok(n) => return Err(bad(&format!("written by a newer Seedwell (format {n}): update Seedwell"))),
            Err(e) => return Err(bad(&e.to_string())),
        };
        let mut damaged = clear;
        while r.remaining() > 0 {
            let Ok(rec) = r.bytes(usize::MAX >> 1, "record") else {
                damaged = true;
                break;
            };
            let opened = if clear { Some(rec.to_vec()) } else { s.seal.open(rec) };
            let Some(rec) = opened else {
                damaged = true;
                continue;
            };
            let rr = &mut Reader::new(&rec);
            let parsed = (|| {
                let received_ms = rr.varint("received").ok()?;
                let plain = if rr.bool("has body").ok()? { DirectBody::from_bytes(rr.bytes(usize::MAX >> 1, "body").ok()?).ok() } else { None };
                let msg = DirectMessage::read(rr).ok()?;
                Some((received_ms, plain, msg))
            })();
            match parsed {
                // Only this device's own and those to it (0.1.8 kept others' it carried, which go).
                Some((received_ms, plain, msg)) => match if msg.author == *me { plain } else if msg.recipient == *me { msg.open(secret) } else { None } {
                    Some(body) => {
                        s.put(Stored { msg, received_ms, body: Some(body) });
                    }
                    None => damaged = true,
                },
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
        put_varint(&mut out, MSGS_FORMAT);
        for s in self.msgs.values() {
            out.extend_from_slice(&record(&own_copy(s, me), &self.seal));
        }
        let tmp = self.path.with_extension("chat.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &self.path)
    }

    fn insert(&mut self, s: Stored, me: &[u8; 32]) -> io::Result<bool> {
        let rec = record(&own_copy(&s, me), &self.seal);
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

/// This device's settings for private messages (`[direct]`).
#[derive(Clone, Debug, Default)]
pub(crate) struct Settings {
    pub(crate) agents: AgentAccess,
    pub(crate) seq: u64,
}

/// One conversation: what this device knows of the other one, and their messages.
struct Conversation {
    pk: [u8; 32],
    entry: ConversationEntry,
    /// The last message of the other device read here.
    read: Key,
    store: Store,
}

fn read_of(text: &str) -> Option<Key> {
    let mut p = text.split(':');
    let (at, seq, mid) = (p.next()?, p.next()?, p.next()?);
    Some((at.parse().ok()?, seq.parse().ok()?, trs_sync_proto::members::invite_id_from_hex(mid)?))
}

fn read_text(k: &Key) -> String {
    if *k == Key::default() { String::new() } else { format!("{}:{}:{}", k.0, k.1, hex(&k.2)) }
}

/// The file of a conversation's messages, and the secret that holds its key.
fn conversation_path(dir: &Path, device: &DeviceId) -> PathBuf {
    dir.join(format!("dm-{device}.chat"))
}

fn conversation_key_name(device: &DeviceId) -> String {
    format!("store-dm-{device}")
}

/// Private messages on this device.
pub struct Direct {
    secret: [u8; 32],
    me: [u8; 32],
    mine: DirectKey,
    dir: PathBuf,
    secrets: Arc<dyn SecretStore>,
    conversations: Mutex<BTreeMap<[u8; 32], Conversation>>,
    pub(crate) settings: Mutex<Settings>,
}

impl Direct {
    /// Opens this device's conversations in `dir`, from `cfg`'s entries. The single file of 0.1.8 is split into
    /// conversations first (`cfg` gets their entries and is marked [`SyncConfig::upgraded`]).
    pub(crate) fn open(dir: &Path, key: &DeviceKey, cfg: &mut SyncConfig, secrets: Arc<dyn SecretStore>) -> io::Result<Direct> {
        let secret = secret_of(key);
        let me = *key.public_key();
        let d = Direct {
            secret,
            me,
            mine: DirectKey::of(key, now_ms()),
            dir: dir.to_path_buf(),
            secrets,
            conversations: Mutex::new(BTreeMap::new()),
            settings: Mutex::new(Settings { agents: AgentAccess::parse(&cfg.direct.agents).unwrap_or_default(), seq: cfg.direct.seq }),
        };
        let mut all = BTreeMap::new();
        for e in &cfg.conversations {
            let Some(pk) = crate::service::public_key_from_hex(&e.device_pk).filter(|pk| DeviceId::of_public_key(pk).to_string() == e.device) else {
                continue;
            };
            match d.open_one(pk, e.clone()) {
                Ok(c) => {
                    all.insert(pk, c);
                }
                Err(err) => eprintln!("seedwell chat: the private conversation with {}: {err}", e.device),
            }
        }
        *lock(&d.conversations) = all;
        d.upgrade(cfg)?;
        Ok(d)
    }

    fn open_one(&self, pk: [u8; 32], entry: ConversationEntry) -> io::Result<Conversation> {
        let device = DeviceId::of_public_key(&pk);
        let seal = Sealer::load(&*self.secrets, &conversation_key_name(&device), MSGS_MAGIC)?;
        let store = Store::open(conversation_path(&self.dir, &device), &self.me, &self.secret, seal)?;
        Ok(Conversation { pk, read: read_of(&entry.read).unwrap_or_default(), entry, store })
    }

    /// 0.1.8's `direct.chat` and `direct.keys`: each conversation in it becomes one of its own (what was carried for
    /// others goes), then both files and their key are deleted.
    fn upgrade(&self, cfg: &mut SyncConfig) -> io::Result<()> {
        let old = self.dir.join("direct.chat");
        if !old.exists() {
            return Ok(());
        }
        let seal = Sealer::load(&*self.secrets, DIRECT_KEY, MSGS_MAGIC)?;
        let store = Store::open(old.clone(), &self.me, &self.secret, seal)?;
        let reads: HashMap<[u8; 32], Key> = cfg
            .direct
            .read
            .iter()
            .filter_map(|t| {
                let (dev, rest) = t.split_once(':')?;
                Some((crate::service::public_key_from_hex(dev)?, read_of(rest)?))
            })
            .collect();
        let names: HashMap<String, String> = cfg.all_devices().map(|d| (d.id.clone(), d.name.clone())).collect();
        let mut all = lock(&self.conversations);
        for s in store.msgs.into_values() {
            let other = if s.msg.author == self.me { s.msg.recipient } else { s.msg.author };
            let c = match all.entry(other) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(v) => {
                    let device = DeviceId::of_public_key(&other).to_string();
                    let entry = ConversationEntry {
                        name: names.get(&device).cloned().unwrap_or_default(),
                        read: reads.get(&other).map(read_text).unwrap_or_default(),
                        device,
                        device_pk: hex(&other),
                        ..ConversationEntry::default()
                    };
                    v.insert(self.open_one(other, entry)?)
                }
            };
            c.store.insert(s, &self.me)?;
        }
        cfg.conversations = all.values().map(|c| c.entry.clone()).collect();
        cfg.direct.read.clear();
        cfg.upgraded = true;
        drop(all);
        std::fs::remove_file(&old)?;
        let _ = std::fs::remove_file(self.dir.join("direct.keys"));
        self.secrets.delete(DIRECT_KEY)?;
        Ok(())
    }

    /// Every conversation's entry, for `sync.toml`.
    pub(crate) fn entries(&self) -> Vec<ConversationEntry> {
        lock(&self.conversations).values().map(|c| ConversationEntry { read: read_text(&c.read), ..c.entry.clone() }).collect()
    }

    /// This device keeps a conversation with `pk`.
    pub(crate) fn has(&self, pk: &[u8; 32]) -> bool {
        lock(&self.conversations).contains_key(pk)
    }

    /// The other devices of the conversations, with their last addresses.
    pub(crate) fn devices(&self) -> Vec<(DeviceId, Vec<std::net::SocketAddr>)> {
        lock(&self.conversations)
            .values()
            .map(|c| (DeviceId::of_public_key(&c.pk), c.entry.addresses.iter().filter_map(|a| a.parse().ok()).collect()))
            .collect()
    }

    /// The names of the conversations' devices, as last seen.
    pub(crate) fn names(&self) -> Vec<(DeviceId, Names)> {
        lock(&self.conversations).values().map(|c| (DeviceId::of_public_key(&c.pk), Names { device: c.entry.name.clone(), user: c.entry.user.clone() })).collect()
    }

    /// Notes what a conversation's device is called now and where it was reached. True when that changed.
    pub(crate) fn note(&self, device: &DeviceId, name: Option<&str>, user: Option<&str>, addr: Option<std::net::SocketAddr>) -> bool {
        let mut all = lock(&self.conversations);
        let Some(c) = all.values_mut().find(|c| DeviceId::of_public_key(&c.pk) == *device) else { return false };
        let e = &mut c.entry;
        let mut changed = false;
        if let Some(n) = name.filter(|n| !n.is_empty() && *n != e.name) {
            e.name = n.to_string();
            changed = true;
        }
        if let Some(u) = user.filter(|u| *u != e.user) {
            e.user = u.to_string();
            changed = true;
        }
        if let Some(a) = addr.map(|a| a.to_string()).filter(|a| e.addresses.first() != Some(a)) {
            e.addresses.retain(|x| *x != a);
            e.addresses.insert(0, a);
            e.addresses.truncate(8);
            changed = true;
        }
        changed
    }

    /// Starts a conversation with `peer` (connected now), when there is none yet.
    fn start_with(&self, peer: &PeerHandle) -> io::Result<bool> {
        if self.has(&peer.device_pk) {
            return Ok(false);
        }
        let entry = ConversationEntry {
            device: peer.device.to_string(),
            device_pk: hex(&peer.device_pk),
            name: peer.name.clone(),
            user: lock(&peer.user_name).clone(),
            addresses: if peer.dialed { vec![peer.remote.to_string()] } else { Vec::new() },
            read: String::new(),
        };
        let c = self.open_one(peer.device_pk, entry)?;
        lock(&self.conversations).insert(peer.device_pk, c);
        Ok(true)
    }

    /// Deletes a conversation: its entry, its messages and their key. False when there was none.
    fn delete(&self, device: &DeviceId) -> io::Result<bool> {
        let gone = {
            let mut all = lock(&self.conversations);
            let pk = all.keys().find(|pk| DeviceId::of_public_key(pk) == *device).copied();
            pk.and_then(|pk| all.remove(&pk))
        };
        let Some(c) = gone else { return Ok(false) };
        drop(c);
        let _ = std::fs::remove_file(conversation_path(&self.dir, device));
        self.secrets.delete(&conversation_key_name(device))?;
        Ok(true)
    }
}

/// A session over which private messages may go: both sides have [`FEATURE_DIRECT`], and no device relays it.
pub(crate) fn direct_ok(peer: &PeerHandle) -> bool {
    peer.agreed.features & FEATURE_DIRECT != 0 && peer.via != crate::session::Via::Relay
}

/// A device that is an active member of some channel this device is in: one a conversation may start with.
fn in_a_channel(ctx: &Ctx, pk: &[u8; 32]) -> bool {
    ctx.registry.channels().iter().any(|c| c.role_of(pk).is_some())
}

/// The conversations changed: `sync.toml` gets their entries.
fn changed(ctx: &Ctx) {
    let _ = ctx.hub.send(HubMsg::Conversations);
}

/// A direct session started: our key, and what we hold of the other's messages (when we keep a conversation with
/// it); what it says of itself is noted.
pub(crate) async fn start(ctx: &Arc<Ctx>, peer: &Arc<PeerHandle>) {
    if !direct_ok(peer) {
        return;
    }
    if ctx.direct.note(&peer.device, Some(&peer.name), Some(&lock(&peer.user_name).clone()), peer.dialed.then_some(peer.remote)) {
        changed(ctx);
    }
    let _ = peer.control.send(Message::DirectKeys(DirectKeys { records: vec![ctx.direct.mine] })).await;
    let have = lock(&ctx.direct.conversations).get(&peer.device_pk).map(|c| c.store.have_for(&ctx.direct.me));
    if let Some(authors) = have {
        let _ = peer.control.send(Message::DirectHave(DirectHave { authors })).await;
    }
}

/// The peer's DirectJoin after the session started (its user renamed itself).
pub(crate) fn on_join(ctx: &Ctx, peer: &PeerHandle, j: DirectJoin) {
    *lock(&peer.user_name) = j.user_name.clone();
    if ctx.direct.note(&peer.device, None, Some(&j.user_name), None) {
        changed(ctx);
    }
}

/// Key records from a peer: only its own counts, and only over a direct session.
pub(crate) fn on_keys(peer: &PeerHandle, records: Vec<DirectKey>) {
    if !direct_ok(peer) {
        return;
    }
    if let Some(k) = records.into_iter().find(|k| k.device == peer.device_pk && k.verify()) {
        *lock(&peer.direct_key) = Some(k);
    }
}

/// The peer said what it holds of our messages to it: send it what it lacks.
pub(crate) async fn on_have(ctx: &Arc<Ctx>, peer: &Arc<PeerHandle>, have: DirectHave) {
    if !direct_ok(peer) {
        return;
    }
    let theirs: HashMap<&[u8; 32], &AuthorHave> = have.authors.iter().map(|a| (&a.author, a)).collect();
    let me = ctx.direct.me;
    if let Some(high) = theirs.get(&me).and_then(|a| a.highest()) {
        let mut s = lock(&ctx.direct.settings);
        s.seq = s.seq.max(high);
    }
    let missing: Vec<DirectMessage> = lock(&ctx.direct.conversations)
        .get(&peer.device_pk)
        .map(|c| c.store.msgs.values().filter(|s| s.msg.author == me && !theirs.get(&me).is_some_and(|a| a.has(s.msg.seq))).map(|s| s.msg.clone()).collect())
        .unwrap_or_default();
    for part in missing.chunks(500) {
        if peer.control.send(Message::DirectMessages(DirectMessages { messages: part.to_vec() })).await.is_err() {
            return;
        }
    }
}

/// Messages from a peer, over a direct session: only those it wrote to this device, which open, in a conversation
/// this device keeps or may start (it shares a channel with the peer). The new ones are told about.
pub(crate) async fn on_messages(ctx: &Arc<Ctx>, peer: &PeerHandle, msgs: Vec<DirectMessage>) {
    if !direct_ok(peer) {
        return;
    }
    let d = &ctx.direct;
    let mut new = Vec::new();
    for m in msgs {
        if m.author != peer.device_pk || m.recipient != d.me || !m.verify() {
            continue;
        }
        let Some(body) = m.open(&d.secret) else { continue };
        if !d.has(&m.author) {
            if !in_a_channel(ctx, &m.author) {
                continue;
            }
            match d.start_with(peer) {
                Ok(_) => changed(ctx),
                Err(e) => {
                    eprintln!("seedwell chat: a private conversation could not be started: {e}");
                    continue;
                }
            }
        }
        let stored = Stored { msg: m.clone(), received_ms: now_ms(), body: Some(body) };
        let res = lock(&d.conversations).get_mut(&m.author).map(|c| c.store.insert(stored, &d.me));
        match res {
            Some(Ok(true)) => new.push(m),
            Some(Ok(false)) | None => {}
            Some(Err(e)) => eprintln!("seedwell chat: private messages could not be saved: {e}"),
        }
    }
    tell(ctx, &new);
}

/// Tells watchers about new messages to or from this device.
fn tell(ctx: &Ctx, new: &[DirectMessage]) {
    if new.is_empty() {
        return;
    }
    let names = crate::chat::seen_names(ctx);
    let agents = lock(&ctx.direct.settings).agents;
    let all = lock(&ctx.direct.conversations);
    for m in new {
        let other = if m.author == ctx.direct.me { m.recipient } else { m.author };
        let Some(c) = all.get(&other) else { continue };
        if let Some(i) = c.store.ids.get(&m.id).and_then(|k| c.store.msgs.get(k)).and_then(|s| info(ctx, s, &names)) {
            let _ = ctx.chat_events.send(ChatEvent { message: i, agents });
        }
    }
}

/// A stored message as the UI sees it.
fn info(ctx: &Ctx, s: &Stored, names: &HashMap<DeviceId, Names>) -> Option<MessageInfo> {
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
        author_name: names.get(&author).map(|n| n.device.clone()).unwrap_or_default(),
        author_user: names.get(&author).map(|n| n.user.clone()).unwrap_or_default(),
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
        trust: crate::chat::trust(mine, b.agent.is_some()).into(),
    })
}

impl SyncHandle {
    /// Writes the conversations' entries into `sync.toml`.
    pub(crate) fn save_conversations(&self) -> Result<(), SyncError> {
        let entries = self.inner.ctx.direct.entries();
        lock(&self.inner.cfg).conversations = entries;
        self.save_config()?;
        Ok(())
    }

    /// Every private conversation on this device, newest first. Never waits for anything slow.
    pub fn chat_directs(&self) -> Vec<DirectStatus> {
        let d = &self.inner.ctx.direct;
        let names = self.names();
        let mut out: Vec<DirectStatus> = lock(&d.conversations)
            .values()
            .map(|c| {
                let device = DeviceId::of_public_key(&c.pk);
                let unread = c.store.msgs.range((std::ops::Bound::Excluded(c.read), std::ops::Bound::Unbounded)).filter(|(_, s)| s.msg.author != d.me).count() as u32;
                DirectStatus {
                    device,
                    name: names.get(&device).map(|n| n.device.clone()).unwrap_or_default(),
                    user: names.get(&device).map(|n| n.user.clone()).unwrap_or_default(),
                    online: self.inner.ctx.registry.peer(&device).is_some_and(|p| direct_ok(&p)),
                    unread,
                    last_message_s: c.store.msgs.keys().next_back().map(|k| k.0 / 1000),
                }
            })
            .collect();
        out.sort_by(|a, b| b.last_message_s.cmp(&a.last_message_s).then_with(|| a.name.cmp(&b.name)));
        out
    }

    /// The private messages with `device`, oldest first: the `limit` newest before `before`, or the newest.
    pub fn chat_direct_messages(&self, device: &DeviceId, before: Option<&str>, limit: usize) -> Result<Vec<MessageInfo>, SyncError> {
        let d = &self.inner.ctx.direct;
        let names = self.names();
        let all = lock(&d.conversations);
        let Some(c) = all.values().find(|c| DeviceId::of_public_key(&c.pk) == *device) else { return Ok(Vec::new()) };
        let end = match before {
            Some(b) => Some(trs_sync_proto::members::invite_id_from_hex(b).and_then(|id| c.store.ids.get(&id).copied()).ok_or_else(|| invalid("no such message"))?),
            None => None,
        };
        let mut page: Vec<MessageInfo> =
            c.store.msgs.iter().rev().filter(|(k, _)| end.is_none_or(|e| **k < e)).take(limit).filter_map(|(_, s)| info(&self.inner.ctx, s, &names)).collect();
        page.reverse();
        Ok(page)
    }

    /// Writes a private message to `device`: only while a direct session with it is open (none relayed), to a
    /// device this one keeps a conversation with or shares a channel with. `agent` as for channels.
    pub async fn chat_send_direct(&self, device: &DeviceId, kind: MessageKind, body: &str, reply_to: Option<&str>, agent: Option<&str>) -> Result<MessageInfo, SyncError> {
        let ctx = &self.inner.ctx;
        let d = &ctx.direct;
        if *device == ctx.me {
            return Err(invalid("a private message goes to another device"));
        }
        let peer = ctx.registry.peer(device);
        let known = peer.as_ref().is_some_and(|p| d.has(&p.device_pk) || in_a_channel(ctx, &p.device_pk))
            || lock(&d.conversations).values().any(|c| DeviceId::of_public_key(&c.pk) == *device);
        if !known {
            return Err(invalid(concat!("this ", trs_platform::computer!(), " has no conversation with that device and shares no channel with it, so it cannot write to it privately")));
        }
        let peer = peer.filter(|p| direct_ok(p)).ok_or_else(|| match ctx.registry.peer(device) {
            Some(p) if p.agreed.features & FEATURE_DIRECT == 0 => invalid("that device runs an older Seedwell: update it to write to it privately"),
            _ => invalid(concat!("that device is not connected to this ", trs_platform::computer!(), " directly; private messages go only over a direct connection between the two, never through another device, so write once both are online and can reach each other")),
        })?;
        let key = lock(&peer.direct_key).ok_or_else(|| invalid("the connection with that device is just starting; try again in a moment"))?;
        if kind == MessageKind::Json && serde_json::from_str::<serde_json::Value>(body).is_err() {
            return Err(invalid("the message is not JSON"));
        }
        if d.start_with(&peer)? {
            self.save_conversations()?;
        }
        let reply_to = match reply_to {
            Some(r) => {
                let id = trs_sync_proto::members::invite_id_from_hex(r);
                let known = id.is_some_and(|id| lock(&d.conversations).get(&peer.device_pk).is_some_and(|c| c.store.ids.contains_key(&id)));
                Some(id.filter(|_| known).ok_or_else(|| invalid("no such message to reply to"))?)
            }
            None => None,
        };
        let mut eph = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut eph).map_err(|_| invalid("no random numbers"))?;
        let plain = DirectBody { reply_to, agent: agent.map(str::to_string), kind, body: body.as_bytes().to_vec() };
        let msg = {
            let mut s = lock(&d.settings);
            let highest = lock(&d.conversations).values().map(|c| c.store.highest(&d.me)).max().unwrap_or(0);
            let seq = s.seq.max(highest) + 1;
            let draft = DirectDraft { id: random_id(), seq, at_ms: now_ms(), body: plain.clone(), ephemeral_secret: eph };
            let msg = DirectMessage::seal(&ctx.key, &peer.device_pk, &key.x25519, draft).map_err(|e| invalid(e.to_string()))?;
            s.seq = seq;
            msg
        };
        let stored = Stored { msg: msg.clone(), received_ms: now_ms(), body: Some(plain) };
        let kept = lock(&d.conversations).get_mut(&peer.device_pk).map(|c| c.store.insert(stored, &d.me)).transpose()?;
        if kept != Some(true) {
            return Err(invalid("the message could not be kept"));
        }
        lock(&self.inner.cfg).direct.seq = msg.seq;
        self.save_config()?;
        let names = self.names();
        let out = {
            let all = lock(&d.conversations);
            all.get(&peer.device_pk).and_then(|c| c.store.ids.get(&msg.id).and_then(|k| c.store.msgs.get(k))).and_then(|s| info(ctx, s, &names))
        };
        let _ = peer.control.send(Message::DirectMessages(DirectMessages { messages: vec![msg.clone()] })).await;
        tell(ctx, std::slice::from_ref(&msg));
        out.ok_or(SyncError::NotFound)
    }

    /// Marks every private message from `device` read.
    pub fn chat_mark_direct_read(&self, device: &DeviceId) -> Result<(), SyncError> {
        let d = &self.inner.ctx.direct;
        let moved = {
            let mut all = lock(&d.conversations);
            let Some(c) = all.values_mut().find(|c| DeviceId::of_public_key(&c.pk) == *device) else { return Ok(()) };
            match c.store.msgs.iter().rev().find(|(_, s)| s.msg.author != d.me).map(|(k, _)| *k) {
                Some(last) if last != c.read => {
                    c.read = last;
                    true
                }
                _ => false,
            }
        };
        if moved {
            self.save_conversations()?;
        }
        Ok(())
    }

    /// Deletes the private conversation with `device` on this Mac: its messages, their key and all this Mac knows
    /// of the device through it. A session that stood for it alone ends. (The other device keeps its copy.)
    pub fn chat_delete_direct(&self, device: &DeviceId) -> Result<(), SyncError> {
        if !self.inner.ctx.direct.delete(device)? {
            return Err(SyncError::NotFound);
        }
        self.save_conversations()?;
        if let Some(p) = self.inner.ctx.registry.peer(device)
            && p.folders.read().unwrap_or_else(|e| e.into_inner()).is_empty()
        {
            p.close();
        }
        self.forget_strangers();
        Ok(())
    }

    /// Tells the direct peers this user's name again (it changed).
    pub(crate) fn send_direct_joins(&self) {
        let ctx = self.inner.ctx.clone();
        let user_name = lock(&ctx.user_name).clone();
        for p in ctx.registry.peers().into_iter().filter(|p| direct_ok(p)) {
            let j = DirectJoin { conversation: ctx.direct.has(&p.device_pk), user_name: user_name.clone() };
            let _ = p.control.try_send(Message::DirectJoin(j));
        }
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

    /// **Tests only:** `device`'s key for private messages came over a direct session open now.
    #[doc(hidden)]
    pub fn test_direct_knows(&self, device: &DeviceId) -> bool {
        self.inner.ctx.registry.peer(device).is_some_and(|p| lock(&p.direct_key).is_some())
    }

    /// **Tests only:** how many private messages this device keeps, in all its conversations.
    #[doc(hidden)]
    pub fn test_direct_held(&self) -> usize {
        lock(&self.inner.ctx.direct.conversations).values().map(|c| c.store.msgs.len()).sum()
    }

    /// **Tests only:** the kept messages in wire form (to tamper with in a test).
    #[doc(hidden)]
    pub fn test_direct_wire(&self) -> Vec<Vec<u8>> {
        lock(&self.inner.ctx.direct.conversations).values().flat_map(|c| c.store.msgs.values().map(|s| s.msg.to_bytes())).collect()
    }

    /// **Tests only:** a private message to `device` (connected directly now), sealed and signed but neither kept
    /// nor sent (wire form, to tamper with in a test).
    #[doc(hidden)]
    pub fn test_direct_seal(&self, device: &DeviceId, seq: u64, body: &str) -> Option<Vec<u8>> {
        let ctx = &self.inner.ctx;
        let peer = ctx.registry.peer(device)?;
        let key = (*lock(&peer.direct_key))?;
        let draft = DirectDraft {
            id: random_id(),
            seq,
            at_ms: now_ms(),
            body: DirectBody { reply_to: None, agent: None, kind: MessageKind::Text, body: body.as_bytes().to_vec() },
            ephemeral_secret: [seq as u8 | 1; 32],
        };
        DirectMessage::seal(&ctx.key, &peer.device_pk, &key.x25519, draft).ok().map(|m| m.to_bytes())
    }

    /// **Tests only:** messages (wire form) arrive over the session with `from` (connected now).
    #[doc(hidden)]
    pub async fn test_direct_receive(&self, from: DeviceId, messages: &[Vec<u8>]) {
        let Some(peer) = self.inner.ctx.registry.peer(&from) else { return };
        let msgs = messages.iter().filter_map(|b| DirectMessage::from_bytes(b).ok()).collect();
        on_messages(&self.inner.ctx, &peer, msgs).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::FileSecrets;
    use trs_sync_proto::msg::{Agreed, FEATURE_CHAT};

    fn draft(n: u8, text: &str) -> DirectDraft {
        DirectDraft {
            id: [n; 16],
            seq: n as u64,
            at_ms: 1_793_491_200_000 + n as u64,
            body: DirectBody { reply_to: None, agent: None, kind: MessageKind::Text, body: text.as_bytes().to_vec() },
            ephemeral_secret: [n | 1; 32],
        }
    }

    #[test]
    fn the_single_file_of_0_1_8_becomes_one_conversation_per_device_and_carried_messages_go() {
        let dir = tempfile::tempdir().unwrap();
        let secrets: Arc<dyn SecretStore> = Arc::new(FileSecrets::new(dir.path().join("secrets")));
        let (a, me, c) = (DeviceKey::from_seed(&[1; 32]), DeviceKey::from_seed(&[2; 32]), DeviceKey::from_seed(&[3; 32]));
        let to = |k: &DeviceKey| DirectKey::of(k, 0).x25519;
        let from_a = DirectMessage::seal(&a, me.public_key(), &to(&me), draft(1, "hello from Anna")).unwrap();
        let mine = DirectMessage::seal(&me, a.public_key(), &to(&a), draft(2, "hello back")).unwrap();
        let carried = DirectMessage::seal(&a, c.public_key(), &to(&c), draft(3, "not for me")).unwrap();
        // As 0.1.8 wrote it: format 1, in the clear, the author's own copy with its body.
        let mut old = MSGS_MAGIC.to_vec();
        put_varint(&mut old, 1);
        for (m, body) in [(&from_a, None), (&mine, Some(draft(2, "hello back").body)), (&carried, None)] {
            let mut rec = Vec::new();
            put_varint(&mut rec, m.at_ms);
            match body {
                Some(b) => {
                    rec.push(1);
                    put_bytes(&mut rec, &b.to_bytes());
                }
                None => rec.push(0),
            }
            m.put(&mut rec);
            put_bytes(&mut old, &rec);
        }
        let sync = dir.path().join("sync");
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::write(sync.join("direct.chat"), &old).unwrap();
        std::fs::write(sync.join("direct.keys"), b"SWDK\x01").unwrap();
        let mut cfg = SyncConfig::default();
        cfg.direct.read = vec![format!("{}:{}:1:{}", hex(a.public_key()), from_a.at_ms, hex(&[1; 16]))];

        let d = Direct::open(&sync, &me, &mut cfg, secrets.clone()).unwrap();
        assert!(cfg.upgraded && cfg.direct.read.is_empty());
        let anna = DeviceId::of_public_key(a.public_key());
        assert_eq!(cfg.conversations.len(), 1, "only the conversation with Anna: what was carried for others is gone");
        assert_eq!((cfg.conversations[0].device.as_str(), cfg.conversations[0].read.as_str()), (anna.to_string().as_str(), format!("{}:1:{}", from_a.at_ms, hex(&[1; 16])).as_str()));
        assert_eq!(lock(&d.conversations)[a.public_key()].store.msgs.len(), 2);
        assert!(!sync.join("direct.chat").exists() && !sync.join("direct.keys").exists());
        let file = std::fs::read(sync.join(format!("dm-{anna}.chat"))).unwrap();
        assert!(!file.windows(5).any(|w| w == b"hello"), "sealed");

        // Opened again from its entry: both messages, each readable.
        drop(d);
        let d = Direct::open(&sync, &me, &mut cfg, secrets).unwrap();
        let all = lock(&d.conversations);
        let texts: Vec<String> = all[a.public_key()].store.msgs.values().map(|s| String::from_utf8(s.body.clone().unwrap().body).unwrap()).collect();
        assert_eq!(texts, ["hello from Anna", "hello back"]);
    }

    #[test]
    fn private_messages_go_only_over_a_direct_session_of_a_device_that_has_the_bit() {
        let peer = |features: u64, via: crate::session::Via| {
            let addr: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
            PeerHandle::new([7; 32], "X".into(), "Seedwell".into(), Agreed { protocol: 2, features }, addr, addr, [0; 32], true, via).0
        };
        assert!(direct_ok(&peer(FEATURE_CHAT | FEATURE_DIRECT, crate::session::Via::Tcp)));
        assert!(!direct_ok(&peer(FEATURE_CHAT | FEATURE_DIRECT, crate::session::Via::Relay)), "never relayed");
        assert!(!direct_ok(&peer(FEATURE_CHAT, crate::session::Via::Tcp)), "0.1.8 carries messages for others");
    }
}
