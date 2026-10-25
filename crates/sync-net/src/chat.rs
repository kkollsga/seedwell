//! Channels (SPEC §7G.7): chat between devices, for people and agents.
//!
//! A channel is a folder without files: a [`Space`] (its key pair, membership, invitations and sharing period,
//! all as for folders) and its messages. Every member keeps every message, up to a cap ([`MAX_MESSAGES`],
//! [`MAX_BYTES`], the oldest dropped first), so a device that was offline catches up from any member. When two
//! devices that both speak channels ([`trs_sync_proto::msg::FEATURE_CHAT`]) connect, each says what it holds
//! ([`ChatHave`]) and the other sends what it lacks; new messages are passed on as they come.
//!
//! A message counts only when its author may post when it arrives (Owner, Admin or Read & write, and the sharing
//! has not ended): a removed or expired member's later messages are ignored, its earlier ones stay where they are.
//!
//! Agents: each channel says what agents on this Mac may do with it ([`AgentAccess`], off by default). While they
//! may read it, a wake-up command can be set: the service runs it for each message from another device, with the
//! message as JSON on its standard input (never in its arguments), one run at a time per channel, 30 s at most.
//!
//! On disk: `sync/<id>.chat`, `"SWCH" ‖ format varint (1) ‖ n × (varint length ‖ received_ms varint ‖ message)`,
//! appended to as messages arrive and written anew when old ones are dropped.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use trs_sync_proto::chat::{AuthorHave, ChatMessage, Draft};
use trs_sync_proto::codec::{Reader, put_bytes, put_varint};
use trs_sync_proto::ids::{DeviceId, FolderId};
use trs_sync_proto::keys::{Access, FolderKey, FolderSecret};
use trs_sync_proto::members::{Body, InviteLink, Membership, Role, Statement, hex};
use trs_sync_proto::msg::{ChatHave, ChatJoin, ChatMessages, JoinChannel, MAX_CHAT_MESSAGES, Members, Message};

pub use trs_sync_proto::chat::MessageKind;

use crate::config::ChannelEntry;
use crate::service::{SyncError, SyncHandle, invalid, public_key_from_hex, random_id};
use crate::shared::{Ctx, HubMsg, PeerHandle, Space, SpaceKind, lock};

/// Messages a channel keeps on each device.
pub const MAX_MESSAGES: usize = 10_000;
/// Bytes of messages a channel keeps on each device.
pub const MAX_BYTES: usize = 50 << 20;
/// A message this far ahead of the clock (milliseconds: ten minutes) is shown at the time it arrived, and marked.
pub const FUTURE_SLACK_MS: u64 = 600_000;
/// How long a wake-up command may run.
pub const WAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Messages waiting for a channel's wake-up command; beyond this the oldest waiting are dropped.
const WAKE_QUEUE: usize = 100;
/// About this much of a channel's messages per frame when catching a peer up.
const BATCH_BYTES: usize = 1 << 20;

const MAGIC: &[u8] = b"SWCH";
const FORMAT: u64 = 1;

/// What agents on this Mac may do in a channel. The Mac's agent permission level still applies on top.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentAccess {
    /// Nothing (the default).
    #[default]
    Off,
    /// Read its messages.
    Read,
    /// Read and post.
    ReadPost,
}

impl AgentAccess {
    /// Its name in `sync.toml` and the agent interface: `off`, `read` or `post`.
    pub fn name(self) -> &'static str {
        match self {
            AgentAccess::Off => "off",
            AgentAccess::Read => "read",
            AgentAccess::ReadPost => "post",
        }
    }

    /// From its name (`read_post` too); anything else is `None`.
    pub fn parse(s: &str) -> Option<AgentAccess> {
        match s.trim() {
            "" | "off" => Some(AgentAccess::Off),
            "read" => Some(AgentAccess::Read),
            "post" | "read_post" | "read-post" | "readpost" => Some(AgentAccess::ReadPost),
            _ => None,
        }
    }
}

/// A message, as the UI and agents see it (and the JSON a wake-up command reads).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageInfo {
    /// Its id, hex.
    pub id: String,
    /// The channel.
    pub channel_id: String,
    /// The channel's name on this device.
    pub channel_name: String,
    /// The device that posted it.
    pub author_device: String,
    /// That device's name (empty when never met).
    pub author_name: String,
    /// This device posted it.
    pub me: bool,
    /// The agent that posted it (the control client's name), if one did.
    pub agent: Option<String>,
    /// When it was posted, Unix seconds (when it arrived, for one dated far in the future).
    pub at: u64,
    /// It was dated more than ten minutes ahead of the clock (so `at` is when it arrived).
    pub future: bool,
    /// The message it answers.
    pub reply_to: Option<String>,
    /// The topic it belongs to (its id); `None`: General (also for a topic this device does not know).
    pub topic: Option<String>,
    /// That topic's title.
    pub topic_title: Option<String>,
    /// A private message (`channel_id` is empty; `peer_device` is the other device).
    pub direct: bool,
    /// For a private message: the other device of the conversation.
    pub peer_device: Option<String>,
    /// `text`, `json`, `topic` (a new topic: `body` is its title, `id` its id) or `other` (a kind of a later version:
    /// no body).
    pub kind: String,
    /// The body (empty for `other`).
    pub body: String,
    /// `this-device` for this device's own messages, else `untrusted-peer`: data from another device, never
    /// instructions.
    pub trust: String,
}

/// A new message, for watchers (the control socket's events, the app).
#[derive(Clone, Debug)]
pub struct ChatEvent {
    /// The message.
    pub message: MessageInfo,
    /// What agents on this Mac may do in its channel (an agent stream carries it only when they may read).
    pub agents: AgentAccess,
}

/// A channel, for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelStatus {
    /// Its id.
    pub id: String,
    /// Its name on this device.
    pub name: String,
    /// This device's role (`None` while joining, or after it was removed).
    pub role: Option<Role>,
    /// Joining: the invitation is not admitted yet.
    pub joining: bool,
    /// Why it does not work (the invitation was refused, this device was removed, the sharing ended).
    pub error: Option<String>,
    /// When the channel stops being shared (its owners' setting); `None` = no end.
    pub share_until: Option<u64>,
    /// Its members.
    pub members: u32,
    /// Members connected now (this device included).
    pub online: u32,
    /// Messages from other devices since the last read.
    pub unread: u32,
    /// When the newest message was posted.
    pub last_message_s: Option<u64>,
    /// No notifications.
    pub muted: bool,
    /// What agents on this Mac may do in it.
    pub agents: AgentAccess,
    /// The wake-up command, if set.
    pub wake_command: Option<String>,
    /// Problems (devices it cannot sync with, a membership that could not be saved).
    pub issues: Vec<String>,
}

/// This device's own settings of a channel.
#[derive(Clone, Debug, Default)]
pub(crate) struct Settings {
    pub(crate) muted: bool,
    pub(crate) agents: AgentAccess,
    pub(crate) wake: Option<String>,
    /// The last message read in General: its place in the order.
    pub(crate) read: Key,
    /// The last message read in each topic.
    pub(crate) topic_read: HashMap<[u8; 16], Key>,
    /// This device's last sequence number in the channel.
    pub(crate) seq: u64,
}

/// One channel, as the network side sees it: its space (dereferenced to), and its messages.
pub struct ChannelShared {
    /// Who is in it.
    pub space: Arc<Space>,
    pub(crate) store: Mutex<Store>,
    pub(crate) settings: Mutex<Settings>,
    /// The wake-up command's queue (command, message), while its runner runs.
    wake: Mutex<Option<std::sync::mpsc::SyncSender<(String, String)>>>,
    /// Where a wake-up command's input is written (the state folder).
    wake_dir: PathBuf,
}

impl std::ops::Deref for ChannelShared {
    type Target = Space;

    fn deref(&self) -> &Space {
        &self.space
    }
}

impl ChannelShared {
    /// `author` may post now (Owner, Admin or Read & write, while the sharing lasts).
    pub fn may_post(&self, author: &[u8; 32]) -> bool {
        self.role_of(author).is_some_and(|r| r >= Role::ReadWrite)
    }
}

// ----------------------------------------------------------------------------------------------------- store

struct Stored {
    msg: ChatMessage,
    received_ms: u64,
}

/// A topic of a channel: made by a topic message.
#[derive(Clone, Debug)]
struct TopicMeta {
    title: String,
    author: [u8; 32],
    created_ms: u64,
}

/// Where a message stands in the order: its time (when it arrived, for one dated far ahead), its author's
/// sequence number (so one author's messages of one second keep their order), then its id.
pub(crate) type Key = (u64, u64, [u8; 16]);

fn shown_at(msg: &ChatMessage, received_ms: u64) -> u64 {
    if msg.at_ms > received_ms.saturating_add(FUTURE_SLACK_MS) { received_ms } else { msg.at_ms }
}

/// A channel's messages on this device: all in memory (at most [`MAX_BYTES`]), appended to a file.
pub(crate) struct Store {
    path: PathBuf,
    msgs: BTreeMap<Key, Stored>,
    ids: HashMap<[u8; 16], Key>,
    seqs: HashMap<[u8; 32], BTreeSet<u64>>,
    topics: HashMap<[u8; 16], TopicMeta>,
    bytes: usize,
    /// Messages older than this were dropped (and are not wanted back).
    floor_at_ms: u64,
    cap: (usize, usize),
}

fn record(msg: &ChatMessage, received_ms: u64) -> Vec<u8> {
    let mut rec = Vec::with_capacity(msg.body.len() + 200);
    put_varint(&mut rec, received_ms);
    msg.put(&mut rec);
    let mut out = Vec::with_capacity(rec.len() + 4);
    put_bytes(&mut out, &rec);
    out
}

impl Store {
    /// Opens (or starts) the store at `path`. A damaged tail (a write cut off) is dropped.
    pub(crate) fn open(path: PathBuf, cap: (usize, usize)) -> io::Result<Store> {
        let mut s = Store { path, msgs: BTreeMap::new(), ids: HashMap::new(), seqs: HashMap::new(), topics: HashMap::new(), bytes: 0, floor_at_ms: 0, cap };
        let bytes = match std::fs::read(&s.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                s.rewrite()?;
                return Ok(s);
            }
            Err(e) => return Err(e),
        };
        let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {why}", s.path.display()));
        let body = bytes.strip_prefix(MAGIC).ok_or_else(|| bad("not a channel file"))?;
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
            match (rr.varint("received"), ChatMessage::read(rr)) {
                (Ok(received), Ok(msg)) => {
                    s.put(msg, received);
                }
                _ => damaged = true,
            }
        }
        if s.prune() || damaged {
            s.rewrite()?;
        }
        Ok(s)
    }

    fn put(&mut self, msg: ChatMessage, received_ms: u64) -> bool {
        if self.ids.contains_key(&msg.id) || self.seqs.get(&msg.author).is_some_and(|q| q.contains(&msg.seq)) {
            return false;
        }
        let key = (shown_at(&msg, received_ms), msg.seq, msg.id);
        if msg.kind == MessageKind::Topic
            && let Some(title) = msg.text()
        {
            self.topics.insert(msg.id, TopicMeta { title: title.to_string(), author: msg.author, created_ms: key.0 });
        }
        self.ids.insert(msg.id, key);
        self.seqs.entry(msg.author).or_default().insert(msg.seq);
        self.bytes += msg.body.len() + msg.extra.len() + 160;
        self.msgs.insert(key, Stored { msg, received_ms });
        true
    }

    /// Over the cap: drops the oldest down to nine tenths of it. True when it dropped some.
    fn prune(&mut self) -> bool {
        let (max_n, max_b) = self.cap;
        if self.msgs.len() <= max_n && self.bytes <= max_b {
            return false;
        }
        while self.msgs.len() > max_n * 9 / 10 || self.bytes > max_b / 10 * 9 {
            let Some((_, old)) = self.msgs.pop_first() else { break };
            self.ids.remove(&old.msg.id);
            if let Some(q) = self.seqs.get_mut(&old.msg.author) {
                q.remove(&old.msg.seq);
            }
            self.bytes = self.bytes.saturating_sub(old.msg.body.len() + old.msg.extra.len() + 160);
            self.floor_at_ms = self.floor_at_ms.max(old.msg.at_ms);
        }
        true
    }

    fn rewrite(&self) -> io::Result<()> {
        let mut out = MAGIC.to_vec();
        put_varint(&mut out, FORMAT);
        for s in self.msgs.values() {
            out.extend_from_slice(&record(&s.msg, s.received_ms));
        }
        let tmp = self.path.with_extension("chat.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &self.path)
    }

    /// Keeps a message (checked by the caller). False when it is known already, or older than what is kept.
    pub(crate) fn insert(&mut self, msg: ChatMessage, received_ms: u64) -> io::Result<bool> {
        if self.floor_at_ms != 0 && msg.at_ms < self.floor_at_ms {
            return Ok(false);
        }
        let rec = record(&msg, received_ms);
        if !self.put(msg, received_ms) {
            return Ok(false);
        }
        if self.prune() {
            self.rewrite()?;
        } else {
            let mut f = std::fs::OpenOptions::new().append(true).open(&self.path)?;
            f.write_all(&rec)?;
        }
        Ok(true)
    }

    /// What this device holds, for a peer.
    pub(crate) fn have(&self, channel: FolderId) -> ChatHave {
        let authors = self.seqs.iter().filter(|(_, q)| !q.is_empty()).map(|(a, q)| AuthorHave::of(*a, q.iter().copied())).collect();
        ChatHave { channel_id: channel, floor_at_ms: self.floor_at_ms, authors }
    }

    /// What a peer that holds `have` lacks, oldest first.
    pub(crate) fn missing_for(&self, have: &ChatHave) -> Vec<ChatMessage> {
        let theirs: HashMap<&[u8; 32], &AuthorHave> = have.authors.iter().map(|a| (&a.author, a)).collect();
        self.msgs
            .values()
            .filter(|s| s.msg.at_ms >= have.floor_at_ms && !theirs.get(&s.msg.author).is_some_and(|a| a.has(s.msg.seq)))
            .map(|s| s.msg.clone())
            .collect()
    }

    /// The highest sequence number of `author` held.
    pub(crate) fn highest(&self, author: &[u8; 32]) -> u64 {
        self.seqs.get(author).and_then(|q| q.last().copied()).unwrap_or(0)
    }

    fn key_of(&self, id: &[u8; 16]) -> Option<Key> {
        self.ids.get(id).copied()
    }

    /// The topic a message shows under: its own if this device knows it, else General (`None`).
    fn topic_of(&self, m: &ChatMessage) -> Option<[u8; 16]> {
        m.topic.filter(|t| self.topics.contains_key(t))
    }

    /// The messages shown under `topic` (`None`: General), in order; topic messages themselves are rows, not
    /// messages.
    fn shown_in(&self, topic: Option<[u8; 16]>) -> impl DoubleEndedIterator<Item = (&Key, &Stored)> {
        self.msgs.iter().filter(move |(_, s)| s.msg.kind != MessageKind::Topic && self.topic_of(&s.msg) == topic)
    }

    /// Messages of others under `topic` after `read`, and the newest one's key.
    fn unread_in(&self, topic: Option<[u8; 16]>, read: Key, me: &[u8; 32]) -> (u32, Option<Key>) {
        let mut n = 0;
        let mut last = None;
        for (k, s) in self.shown_in(topic) {
            last = Some(*k);
            if *k > read && s.msg.author != *me {
                n += 1;
            }
        }
        (n, last)
    }
}

// --------------------------------------------------------------------------------------------- service API

fn now_s() -> u64 {
    crate::roles::now_s()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn id_from_hex(s: &str) -> Option<[u8; 16]> {
    trs_sync_proto::members::invite_id_from_hex(s)
}

fn settings_of(e: &ChannelEntry) -> Settings {
    let topic_read = e
        .topic_read
        .iter()
        .filter_map(|t| {
            let mut p = t.split(':');
            let (tid, at, seq, mid) = (p.next()?, p.next()?, p.next()?, p.next()?);
            Some((id_from_hex(tid)?, (at.parse().ok()?, seq.parse().ok()?, id_from_hex(mid)?)))
        })
        .collect();
    Settings {
        muted: e.muted,
        agents: AgentAccess::parse(&e.agents).unwrap_or_default(),
        wake: Some(e.wake.trim().to_string()).filter(|w| !w.is_empty()),
        read: (e.read_at, e.read_seq, id_from_hex(&e.read_id).unwrap_or([0; 16])),
        topic_read,
        seq: e.seq,
    }
}

/// A topic of a channel, for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicStatus {
    /// Its id (the topic message's).
    pub id: String,
    /// The channel.
    pub channel_id: String,
    /// Its title.
    pub title: String,
    /// Who made it.
    pub author_name: String,
    /// When it was made, Unix seconds.
    pub created_s: u64,
    /// When its newest message was posted.
    pub last_message_s: Option<u64>,
    /// Messages of others since the last read in it.
    pub unread: u32,
    /// Its messages.
    pub messages: u32,
}

impl SyncHandle {
    fn chat_path(&self, id: &FolderId) -> PathBuf {
        self.inner.dir.join(format!("{id}.chat"))
    }

    fn channel(&self, id: &FolderId) -> Result<Arc<ChannelShared>, SyncError> {
        self.inner.ctx.registry.channel(id).ok_or(SyncError::NotFound)
    }

    /// Opens a configured channel.
    pub(crate) async fn open_channel(&self, e: &ChannelEntry) -> Result<(), SyncError> {
        let id: FolderId = e.id.parse().map_err(|_| invalid("bad channel id"))?;
        let pk = public_key_from_hex(&e.channel_pk).filter(|pk| FolderId::of_public_key(pk) == id).ok_or_else(|| invalid("the channel's key is missing"))?;
        let secret = match self.inner.secrets.get(&crate::roles::secret_name(&id))? {
            None => None,
            Some(b) => {
                let s = FolderSecret::from_bytes(b.try_into().map_err(|_| invalid("the channel's owner key is damaged"))?);
                if *s.public_key() != pk {
                    return Err(invalid("the channel's owner key does not fit the channel"));
                }
                Some(s)
            }
        };
        let invite = match self.inner.secrets.get(&crate::roles::invite_name(&id))? {
            Some(b) => String::from_utf8(b).ok().and_then(|t| InviteLink::parse_chat(&t).ok()),
            None => None,
        };
        let links: BTreeMap<[u8; 16], String> = match self.inner.secrets.get(&crate::roles::links_name(&id))? {
            Some(b) => String::from_utf8_lossy(&b).lines().filter_map(|l| InviteLink::parse_chat(l).ok().map(|k| (k.invite.id, l.to_string()))).collect(),
            None => BTreeMap::new(),
        };
        let members_path = self.members_path(&id);
        let membership = crate::roles::load(&members_path, pk)?;
        let path = self.chat_path(&id);
        let store = tokio::task::spawn_blocking(move || Store::open(path, (MAX_MESSAGES, MAX_BYTES))).await.map_err(io::Error::other)??;
        let space = Arc::new(Space::new(id, SpaceKind::Channel, FolderKey::Member(pk), secret, membership, members_path, invite, links, e.name.clone()));
        let ch = Arc::new(ChannelShared {
            space,
            store: Mutex::new(store),
            settings: Mutex::new(settings_of(e)),
            wake: Mutex::new(None),
            wake_dir: self.inner.dir.clone(),
        });
        {
            let cfg = lock(&self.inner.cfg);
            let mut members = ch.members.write().unwrap_or_else(|e| e.into_inner());
            let mut book = lock(&self.inner.book);
            for d in cfg.devices.iter().filter(|d| d.folders.contains(&e.id)) {
                let Ok(dev) = d.id.parse::<DeviceId>() else { continue };
                members.insert(dev);
                for a in &d.addresses {
                    if let Ok(sa) = a.parse() {
                        book.by_device.entry(dev).or_default().insert(sa);
                    }
                }
            }
            for a in &e.addresses {
                if let Ok(sa) = a.parse() {
                    book.hints.entry(id).or_default().insert(sa);
                }
            }
        }
        self.inner.ctx.registry.add_channel(ch);
        self.inner.wake_dialer.notify_one();
        self.inner.lan.wake.notify_one();
        self.offer_channel(id).await;
        Ok(())
    }

    /// Offers a channel opened now to every connected device that speaks channels (as for folders: a device that
    /// does not hold it learns only its id).
    async fn offer_channel(&self, id: FolderId) {
        for p in self.inner.ctx.registry.peers() {
            if !p.chat() || p.access(&id).is_some() {
                continue;
            }
            let join = own_chat_join(&self.inner.ctx, &[id], &p.exporter);
            if !join.channels.is_empty() {
                let _ = p.control.send(Message::ChatJoin(join)).await;
            }
        }
    }

    fn save_channel(&self, entry: ChannelEntry) -> Result<(), SyncError> {
        {
            let mut cfg = lock(&self.inner.cfg);
            cfg.channels.retain(|c| c.id != entry.id);
            cfg.channels.push(entry);
        }
        self.save_config()?;
        Ok(())
    }

    /// Makes a channel; this device is its owner.
    pub async fn chat_new(&self, name: &str) -> Result<ChannelStatus, SyncError> {
        let name = clean_name(name).ok_or_else(|| invalid("a channel needs a name"))?;
        let secret = FolderSecret::generate(&mut ring::rand::SystemRandom::new()).map_err(io::Error::other)?;
        let id = FolderId::of_public_key(secret.public_key());
        let me = *self.inner.ctx.key.public_key();
        let mut m = Membership::new(*secret.public_key());
        m.add([Statement::by_owner(&secret, Body::Grant { device: me, role: Role::Owner, serial: 1, at_s: now_s(), invite: None })]);
        crate::roles::save(&self.members_path(&id), &m)?;
        self.inner.secrets.put(&crate::roles::secret_name(&id), secret.as_bytes())?;
        let entry = ChannelEntry { id: id.to_string(), channel_pk: hex(secret.public_key()), name, ..ChannelEntry::default() };
        self.save_channel(entry.clone())?;
        self.open_channel(&entry).await?;
        self.channel_status(&id).ok_or(SyncError::NotFound)
    }

    /// Joins a channel with its invitation link. It waits until one of its owners or admins is online and admits
    /// this device.
    pub async fn chat_join(&self, link: &str) -> Result<ChannelStatus, SyncError> {
        self.chat_join_link(InviteLink::parse_chat(link).map_err(|e| invalid(e.to_string()))?).await
    }

    /// Joins a channel with its parsed invitation link (also the channel part of a folder's link for both).
    pub(crate) async fn chat_join_link(&self, l: InviteLink) -> Result<ChannelStatus, SyncError> {
        if l.invite.expired(now_s()) {
            return Err(invalid("this link has expired"));
        }
        let id = l.invite.folder_id();
        if self.inner.ctx.registry.channel(&id).is_some() {
            return Err(invalid("this channel is already on this Mac"));
        }
        self.inner.secrets.put(&crate::roles::invite_name(&id), l.format_chat().as_bytes())?;
        let name = l.name.clone().and_then(|n| clean_name(&n)).unwrap_or_else(|| "Channel".into());
        let entry = ChannelEntry { id: id.to_string(), channel_pk: hex(&l.invite.folder_pk), name, ..ChannelEntry::default() };
        self.save_channel(entry.clone())?;
        self.open_channel(&entry).await?;
        self.channel_status(&id).ok_or(SyncError::NotFound)
    }

    /// Leaves a channel on this device: its messages and keys here go (other members keep theirs).
    pub fn chat_leave(&self, id: &FolderId) -> Result<(), SyncError> {
        let known = lock(&self.inner.cfg).channels.iter().any(|c| c.id == id.to_string());
        if self.inner.ctx.registry.remove_channel(id).is_none() && !known {
            return Err(SyncError::NotFound);
        }
        for p in self.inner.ctx.registry.peers() {
            p.folders.write().unwrap_or_else(|e| e.into_inner()).remove(id);
        }
        lock(&self.inner.cfg).channels.retain(|c| c.id != id.to_string());
        self.save_config()?;
        for name in [crate::roles::secret_name(id), crate::roles::invite_name(id), crate::roles::links_name(id)] {
            self.inner.secrets.delete(&name)?;
        }
        let _ = std::fs::remove_file(self.members_path(id));
        let _ = std::fs::remove_file(self.chat_path(id));
        Ok(())
    }

    /// Every channel on this device, by name. Never waits for anything slow.
    pub fn chat_channels(&self) -> Vec<ChannelStatus> {
        let mut out: Vec<ChannelStatus> = self.inner.ctx.registry.channels().iter().map(|c| self.status_of_channel(c)).collect();
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
        out
    }

    /// One channel.
    pub fn channel_status(&self, id: &FolderId) -> Option<ChannelStatus> {
        self.inner.ctx.registry.channel(id).map(|c| self.status_of_channel(&c))
    }

    fn status_of_channel(&self, c: &ChannelShared) -> ChannelStatus {
        let me = *self.inner.ctx.key.public_key();
        let (role, share_until, members, expired) = {
            let m = c.membership.read().unwrap_or_else(|e| e.into_inner());
            (m.role(&me), m.share_until(), m.members().count() as u32, m.expired(&me))
        };
        let joining = lock(&c.invite).is_some();
        let refused = lock(&c.notes).refused.clone();
        let error = match (role, joining) {
            (Some(_), _) => None,
            (None, true) => refused,
            (None, false) => Some(if expired { "this channel is no longer shared" } else { "this Mac was removed from the channel" }.into()),
        };
        let online = 1 + self.inner.ctx.registry.peers().iter().filter(|p| p.access(&c.id).is_some()).count() as u32;
        let s = lock(&c.settings).clone();
        let (unread, last) = {
            let store = lock(&c.store);
            // General's, and each topic's against its own mark: the channel's total.
            let mut unread = store.unread_in(None, s.read, &me).0;
            for t in store.topics.keys() {
                unread += store.unread_in(Some(*t), s.topic_read.get(t).copied().unwrap_or_default(), &me).0;
            }
            (unread, store.msgs.values().next_back().map(|m| shown_at(&m.msg, m.received_ms) / 1000))
        };
        ChannelStatus {
            id: c.id.to_string(),
            name: lock(&c.name).clone(),
            role,
            joining: joining && role.is_none(),
            error,
            share_until,
            members,
            online,
            unread,
            last_message_s: last,
            muted: s.muted,
            agents: s.agents,
            wake_command: s.wake.clone(),
            issues: c.note_issues().into_iter().map(|(_, w)| w).collect(),
        }
    }

    fn names(&self) -> HashMap<DeviceId, String> {
        let mut out: HashMap<DeviceId, String> =
            lock(&self.inner.cfg).devices.iter().filter_map(|d| Some((d.id.parse().ok()?, d.name.clone()))).collect();
        for p in self.inner.ctx.registry.peers() {
            out.insert(p.device, p.name.clone());
        }
        out.insert(self.inner.ctx.me, lock(&self.inner.ctx.device_name).clone());
        out
    }

    /// A page of a channel's messages under `topic` (`None`: General), oldest first: the `limit` newest before
    /// `before` (a message id), or the newest. Never waits for anything slow.
    pub fn chat_messages(&self, id: &FolderId, topic: Option<&str>, before: Option<&str>, limit: usize) -> Result<Vec<MessageInfo>, SyncError> {
        let c = self.channel(id)?;
        let names = self.names();
        let store = lock(&c.store);
        let topic = topic_id(&store, topic)?;
        let end = match before {
            Some(b) => Some(store.key_of(&id_from_hex(b).ok_or_else(|| invalid("no such message"))?).ok_or_else(|| invalid("no such message"))?),
            None => None,
        };
        let mut page: Vec<MessageInfo> = store
            .shown_in(topic)
            .rev()
            .filter(|(k, _)| end.is_none_or(|e| **k < e))
            .take(limit)
            .map(|(_, s)| info(&self.inner.ctx, &c, &store, s, &names))
            .collect();
        page.reverse();
        Ok(page)
    }

    /// A channel's messages after `since` (a message id), or its `last` newest (all when neither is given), oldest
    /// first: every message, topics' and new topics included, or those of one topic (`Some(None)`: General).
    pub fn chat_read(&self, id: &FolderId, topic: Option<Option<&str>>, since: Option<&str>, last: Option<usize>) -> Result<Vec<MessageInfo>, SyncError> {
        let c = self.channel(id)?;
        let names = self.names();
        let store = lock(&c.store);
        let topic = topic.map(|t| topic_id(&store, t)).transpose()?;
        let start = match since {
            Some(s) => {
                let k = store.key_of(&id_from_hex(s).ok_or_else(|| invalid("no such message"))?).ok_or_else(|| invalid("no such message (it may be too old)"))?;
                std::ops::Bound::Excluded(k)
            }
            None => std::ops::Bound::Unbounded,
        };
        let all: Vec<&Stored> = store
            .msgs
            .range((start, std::ops::Bound::Unbounded))
            .map(|(_, s)| s)
            .filter(|s| topic.is_none_or(|t| s.msg.kind != MessageKind::Topic && store.topic_of(&s.msg) == t))
            .collect();
        let skip = last.map_or(0, |n| all.len().saturating_sub(n));
        Ok(all[skip..].iter().map(|s| info(&self.inner.ctx, &c, &store, s, &names)).collect())
    }

    /// A channel's topics, newest activity first (General is not one).
    pub fn chat_topics(&self, id: &FolderId) -> Result<Vec<TopicStatus>, SyncError> {
        let c = self.channel(id)?;
        let names = self.names();
        let me = *self.inner.ctx.key.public_key();
        let marks = lock(&c.settings).topic_read.clone();
        let store = lock(&c.store);
        let mut out: Vec<(u64, TopicStatus)> = store
            .topics
            .iter()
            .map(|(tid, t)| {
                let (unread, last) = store.unread_in(Some(*tid), marks.get(tid).copied().unwrap_or_default(), &me);
                let messages = store.shown_in(Some(*tid)).count() as u32;
                let activity = last.map_or(t.created_ms, |k| k.0.max(t.created_ms));
                let status = TopicStatus {
                    id: hex(tid),
                    channel_id: c.id.to_string(),
                    title: t.title.clone(),
                    author_name: names.get(&DeviceId::of_public_key(&t.author)).cloned().unwrap_or_default(),
                    created_s: t.created_ms / 1000,
                    last_message_s: last.map(|k| k.0 / 1000),
                    unread,
                    messages,
                };
                (activity, status)
            })
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        Ok(out.into_iter().map(|(_, t)| t).collect())
    }

    /// Makes a topic in a channel (anyone who may post): a topic message whose id becomes the topic's.
    pub async fn chat_new_topic(&self, id: &FolderId, title: &str, agent: Option<&str>) -> Result<TopicStatus, SyncError> {
        let title = clean_name(title).ok_or_else(|| invalid("a topic needs a title"))?;
        let m = self.chat_send(id, None, MessageKind::Topic, &title, None, agent).await?;
        self.chat_topics(id)?.into_iter().find(|t| t.id == m.id).ok_or(SyncError::NotFound)
    }

    /// Posts a message to a channel as this device; `agent` names the control client that posts it (the service's
    /// callers set it, never the client). Owners, admins and Read & write members post.
    pub async fn chat_send(&self, id: &FolderId, topic: Option<&str>, kind: MessageKind, body: &str, reply_to: Option<&str>, agent: Option<&str>) -> Result<MessageInfo, SyncError> {
        let c = self.channel(id)?;
        let topic = topic_id(&lock(&c.store), topic)?;
        let me = *self.inner.ctx.key.public_key();
        if !c.may_post(&me) {
            return Err(invalid(match c.role_of(&me) {
                Some(_) => "Read only members of a channel cannot post",
                None => "this Mac is not a member of the channel (yet)",
            }));
        }
        if kind == MessageKind::Json && serde_json::from_str::<serde_json::Value>(body).is_err() {
            return Err(invalid("the message is not JSON"));
        }
        let reply_to = match reply_to {
            Some(r) => Some(id_from_hex(r).filter(|r| lock(&c.store).ids.contains_key(r)).ok_or_else(|| invalid("no such message to reply to"))?),
            None => None,
        };
        let msg = {
            let mut s = lock(&c.settings);
            let seq = s.seq.max(lock(&c.store).highest(&me)) + 1;
            let draft = Draft { id: random_id(), seq, at_ms: now_ms(), reply_to, topic, agent: agent.map(str::to_string), kind, body: body.as_bytes().to_vec() };
            let msg = ChatMessage::sign(&self.inner.ctx.key, id, draft).map_err(|e| invalid(e.to_string()))?;
            s.seq = seq;
            msg
        };
        let received = now_ms();
        let stored = lock(&c.store).insert(msg.clone(), received)?;
        if !stored {
            return Err(invalid("the message could not be kept"));
        }
        if let Some(e) = lock(&self.inner.cfg).channel_mut(&id.to_string()) {
            e.seq = msg.seq;
        }
        self.save_config()?;
        let names = self.names();
        let out = info(&self.inner.ctx, &c, &lock(&c.store), &Stored { msg: msg.clone(), received_ms: received }, &names);
        announce(&self.inner.ctx, &c, &[msg], None, &names).await;
        Ok(out)
    }

    /// Marks every message of a channel's topic (`None`: General) read.
    pub fn chat_mark_read(&self, id: &FolderId, topic: Option<&str>) -> Result<(), SyncError> {
        let c = self.channel(id)?;
        let (topic, last) = {
            let store = lock(&c.store);
            let topic = topic_id(&store, topic)?;
            (topic, store.shown_in(topic).next_back().map(|(k, _)| *k))
        };
        let Some(last) = last else { return Ok(()) };
        {
            let mut s = lock(&c.settings);
            let mark = match topic {
                None => &mut s.read,
                Some(t) => s.topic_read.entry(t).or_default(),
            };
            if *mark == last {
                return Ok(());
            }
            *mark = last;
        }
        let s = lock(&c.settings).clone();
        if let Some(e) = lock(&self.inner.cfg).channel_mut(&id.to_string()) {
            e.read_at = s.read.0;
            e.read_seq = s.read.1;
            e.read_id = hex(&s.read.2);
            e.topic_read = s.topic_read.iter().map(|(t, k)| format!("{}:{}:{}:{}", hex(t), k.0, k.1, hex(&k.2))).collect();
            e.topic_read.sort();
        }
        self.save_config()?;
        Ok(())
    }

    /// Mutes or unmutes a channel's notifications.
    pub fn chat_set_muted(&self, id: &FolderId, muted: bool) -> Result<(), SyncError> {
        let c = self.channel(id)?;
        lock(&c.settings).muted = muted;
        if let Some(e) = lock(&self.inner.cfg).channel_mut(&id.to_string()) {
            e.muted = muted;
        }
        self.save_config()?;
        Ok(())
    }

    /// Sets what agents on this Mac may do in a channel.
    pub fn chat_set_agents(&self, id: &FolderId, access: AgentAccess) -> Result<(), SyncError> {
        let c = self.channel(id)?;
        lock(&c.settings).agents = access;
        if let Some(e) = lock(&self.inner.cfg).channel_mut(&id.to_string()) {
            e.agents = access.name().into();
        }
        self.save_config()?;
        Ok(())
    }

    /// Sets the wake-up command of a channel (`None` or empty: none). It runs only while agents may read the channel.
    pub fn chat_set_wake_command(&self, id: &FolderId, command: Option<&str>) -> Result<(), SyncError> {
        let c = self.channel(id)?;
        let command = command.map(str::trim).filter(|c| !c.is_empty());
        if command.is_some_and(|c| c.len() > 1024 || c.contains(['\n', '\r', '\0'])) {
            return Err(invalid("a wake-up command is one line of at most 1024 bytes"));
        }
        lock(&c.settings).wake = command.map(str::to_string);
        if let Some(e) = lock(&self.inner.cfg).channel_mut(&id.to_string()) {
            e.wake = command.unwrap_or("").into();
        }
        self.save_config()?;
        Ok(())
    }

    /// Renames a channel on this device.
    pub fn chat_rename(&self, id: &FolderId, name: &str) -> Result<(), SyncError> {
        let c = self.channel(id)?;
        let name = clean_name(name).ok_or_else(|| invalid("a channel needs a name"))?;
        *lock(&c.name) = name.clone();
        if let Some(e) = lock(&self.inner.cfg).channel_mut(&id.to_string()) {
            e.name = name;
        }
        self.save_config()?;
        Ok(())
    }

    /// New messages (this device's and others'), as they come.
    pub fn chat_subscribe(&self) -> tokio::sync::broadcast::Receiver<ChatEvent> {
        self.inner.ctx.chat_events.subscribe()
    }

    /// **Tests only:** a text message signed by this device for a channel, whatever its role, in wire form.
    #[doc(hidden)]
    pub fn test_sign(&self, id: &FolderId, seq: u64, body: &str) -> Vec<u8> {
        self.test_sign_draft(id, Draft { id: random_id(), seq, at_ms: now_ms(), reply_to: None, topic: None, agent: None, kind: MessageKind::Text, body: body.as_bytes().to_vec() })
    }

    /// **Tests only:** a message for channel `id` as `draft` says, signed by this device (wire form).
    #[doc(hidden)]
    pub fn test_sign_draft(&self, id: &FolderId, draft: Draft) -> Vec<u8> {
        ChatMessage::sign(&self.inner.ctx.key, id, draft).map(|m| m.to_bytes()).unwrap_or_default()
    }

    /// **Tests only:** messages (wire form) arrive for a channel as if a peer passed them on.
    #[doc(hidden)]
    pub async fn test_receive(&self, id: &FolderId, from: DeviceId, messages: &[Vec<u8>]) -> Result<(), SyncError> {
        let c = self.channel(id)?;
        let msgs = messages.iter().filter_map(|b| ChatMessage::from_bytes(b).ok()).collect();
        on_messages(&self.inner.ctx, from, &c, msgs).await;
        Ok(())
    }
}

fn clean_name(name: &str) -> Option<String> {
    let n: String = name.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(100).collect();
    (!n.is_empty()).then_some(n)
}

/// A topic named by a caller (`None`, empty or `general`: General), which must exist: its id, a prefix of it (4 or
/// more characters) or its title, when only one topic matches.
fn topic_id(store: &Store, topic: Option<&str>) -> Result<Option<[u8; 16]>, SyncError> {
    let Some(t) = topic.map(str::trim).filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case("general")) else { return Ok(None) };
    if let Some(id) = id_from_hex(t).filter(|id| store.topics.contains_key(id)) {
        return Ok(Some(id));
    }
    let lower = t.to_lowercase();
    let prefix = t.len() >= 4 && t.chars().all(|c| c.is_ascii_hexdigit());
    let hits: Vec<[u8; 16]> = store
        .topics
        .iter()
        .filter(|(id, m)| m.title.to_lowercase() == lower || (prefix && hex(&id[..]).starts_with(&lower)))
        .map(|(id, _)| *id)
        .collect();
    match hits.as_slice() {
        [id] => Ok(Some(*id)),
        [] => Err(invalid("no such topic in this channel")),
        _ => Err(invalid("more than one topic matches: give its id")),
    }
}

fn info(ctx: &Ctx, c: &ChannelShared, store: &Store, s: &Stored, names: &HashMap<DeviceId, String>) -> MessageInfo {
    let m = &s.msg;
    let author = DeviceId::of_public_key(&m.author);
    let me = author == ctx.me;
    let at_ms = shown_at(m, s.received_ms);
    let (kind, body) = match m.kind {
        MessageKind::Text => ("text", m.text().unwrap_or_default().to_string()),
        MessageKind::Json => ("json", m.text().unwrap_or_default().to_string()),
        MessageKind::Topic => ("topic", m.text().unwrap_or_default().to_string()),
        MessageKind::Unknown(_) => ("other", String::new()),
    };
    let topic = store.topic_of(m);
    MessageInfo {
        id: hex(&m.id),
        channel_id: c.id.to_string(),
        channel_name: lock(&c.name).clone(),
        author_device: author.to_string(),
        author_name: names.get(&author).cloned().unwrap_or_default(),
        me,
        agent: m.agent.clone(),
        at: at_ms / 1000,
        future: at_ms != m.at_ms,
        reply_to: m.reply_to.map(|r| hex(&r)),
        topic: topic.map(|t| hex(&t)),
        topic_title: topic.and_then(|t| store.topics.get(&t)).map(|t| t.title.clone()),
        direct: false,
        peer_device: None,
        kind: kind.into(),
        body,
        trust: if me { "this-device" } else { "untrusted-peer" }.into(),
    }
}

/// Tells watchers about new messages, wakes the agent of the channel, and passes them on to every other
/// connected member (`from`: the device they came from, which has them).
async fn announce(ctx: &Arc<Ctx>, c: &Arc<ChannelShared>, new: &[ChatMessage], from: Option<DeviceId>, names: &HashMap<DeviceId, String>) {
    if new.is_empty() {
        return;
    }
    let settings = lock(&c.settings).clone();
    for m in new {
        let i = info(ctx, c, &lock(&c.store), &Stored { msg: m.clone(), received_ms: now_ms() }, names);
        if !i.me && settings.agents >= AgentAccess::Read
            && let Some(cmd) = &settings.wake
            && let Ok(json) = serde_json::to_string(&i)
        {
            wake(c, cmd, json);
        }
        let _ = ctx.chat_events.send(ChatEvent { message: i, agents: settings.agents });
    }
    for p in ctx.registry.peers() {
        if Some(p.device) == from || !p.chat() || p.access(&c.id).is_none() {
            continue;
        }
        for part in batches(new.to_vec()) {
            let _ = p.control.send(Message::ChatMessages(ChatMessages { channel_id: c.id, messages: part })).await;
        }
    }
}

/// Splits messages into frames of about [`BATCH_BYTES`] and at most [`MAX_CHAT_MESSAGES`].
fn batches(msgs: Vec<ChatMessage>) -> Vec<Vec<ChatMessage>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut bytes = 0;
    for m in msgs {
        let size = m.body.len() + m.extra.len() + 200;
        if !cur.is_empty() && (bytes + size > BATCH_BYTES || cur.len() >= MAX_CHAT_MESSAGES) {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        bytes += size;
        cur.push(m);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// -------------------------------------------------------------------------------------------- wake-up command

/// How long a wake-up command may run (short under test).
fn wake_timeout() -> Duration {
    if cfg!(test) { Duration::from_secs(2) } else { WAKE_TIMEOUT }
}

/// Queues a message for the channel's wake-up command, starting its runner when none runs.
fn wake(c: &ChannelShared, command: &str, json: String) {
    let mut q = lock(&c.wake);
    let mut job = (command.to_string(), json);
    if let Some(tx) = q.as_ref() {
        match tx.try_send(job) {
            // Queued; or the command is slow and many are waiting, and this one is dropped (the agent reads the
            // channel anyway).
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => return,
            Err(std::sync::mpsc::TrySendError::Disconnected(j)) => job = j,
        }
    }
    *q = wake_start(c.id, c.wake_dir.clone(), job);
}

/// Starts a channel's runner with its first job: one run at a time, each with its own command (a changed command
/// applies to the next message). It ends when nothing has come for a minute.
fn wake_start(id: FolderId, dir: PathBuf, job: (String, String)) -> Option<std::sync::mpsc::SyncSender<(String, String)>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<(String, String)>(WAKE_QUEUE);
    let _ = tx.try_send(job);
    let spawned = std::thread::Builder::new().name("seedwell-chat-wake".into()).spawn(move || {
        while let Ok((command, json)) = rx.recv_timeout(Duration::from_secs(60)) {
            if let Err(e) = run_wake(&command, &id, &json, &dir, wake_timeout()) {
                eprintln!("seedwell chat: the wake-up command of channel {id} failed: {e}");
            }
        }
    });
    spawned.ok().map(|_| tx)
}

/// Runs `command` with `json` on its standard input (never in its arguments), for at most `timeout`; then it is
/// stopped with everything it started.
///
/// The input is a file (written to `dir`, 0600, and unlinked once the command has it open), not a pipe: a
/// command that never reads its input cannot block the writer, and one that exits early cannot raise SIGPIPE in
/// the app (which, unlike a Rust binary, does not ignore it).
pub(crate) fn run_wake(command: &str, channel: &FolderId, json: &str, dir: &std::path::Path, timeout: Duration) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    let path = dir.join(format!(".wake-{}.json", hex(&random_id())));
    {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
        f.write_all(json.as_bytes())?;
        f.write_all(b"\n")?;
    }
    let input = std::fs::File::open(&path);
    let _ = std::fs::remove_file(&path);
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .env("SEEDWELL_CHANNEL", channel.to_string())
        .stdin(Stdio::from(input?))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group, so a stop reaches what it started too.
        .process_group(0)
        .spawn()?;
    let end = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() { Ok(()) } else { Err(io::Error::other(format!("it ended with {status}"))) };
        }
        if std::time::Instant::now() >= end {
            let _ = Command::new("/bin/kill").args(["-KILL", "--", &format!("-{}", child.id())]).stderr(Stdio::null()).status();
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(format!("it ran for more than {} seconds and was stopped", timeout.as_secs())));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ------------------------------------------------------------------------------------------------- sessions

/// Our ChatJoin for these channels (those we are a member of, or join with an invitation), with our proofs bound
/// to the session.
pub(crate) fn own_chat_join(ctx: &Ctx, ids: &[FolderId], exporter: &[u8; 32]) -> ChatJoin {
    let channels = ids
        .iter()
        .filter_map(|id| ctx.registry.channel(id))
        .filter_map(|c| Some(JoinChannel { channel_id: c.id, proofs: crate::roles::proofs(ctx, &c, exporter)? }))
        .collect();
    ChatJoin { channels }
}

/// What a peer's ChatJoin proved: (channel, its access) for each it may share, invitations refused and why, and
/// the channels whose membership changed.
pub(crate) struct CheckedChat {
    pub(crate) shared: Vec<(FolderId, Access)>,
    pub(crate) refused: Vec<String>,
    pub(crate) changed: Vec<FolderId>,
}

/// Checks a peer's ChatJoin (as [`crate::peer`] does for folders). `answered_only` (non-empty for a dialer) limits
/// it to the channels we asked about.
pub(crate) fn check_chat_join(ctx: &Ctx, j: &ChatJoin, peer_pk: &[u8; 32], exporter: &[u8; 32], answered_only: &BTreeSet<FolderId>) -> CheckedChat {
    let mut out = CheckedChat { shared: Vec::new(), refused: Vec::new(), changed: Vec::new() };
    for jc in &j.channels {
        if !answered_only.is_empty() && !answered_only.contains(&jc.channel_id) {
            continue;
        }
        let Some(c) = ctx.registry.channel(&jc.channel_id) else { continue };
        let mut changed = false;
        match crate::roles::check(ctx, &c, peer_pk, exporter, &jc.proofs, &mut changed) {
            crate::roles::Proved::Member(a) => out.shared.push((jc.channel_id, a)),
            crate::roles::Proved::Refused(why) => out.refused.push(why),
            crate::roles::Proved::Nothing => {}
        }
        if changed {
            out.changed.push(jc.channel_id);
        }
    }
    out
}

/// Starts sharing a channel over a session: the membership first, then what we hold.
pub(crate) fn start_chat_link(ctx: &Arc<Ctx>, peer: &Arc<PeerHandle>, id: FolderId, access: Access) -> tokio::task::JoinHandle<()> {
    peer.folders.write().unwrap_or_else(|e| e.into_inner()).insert(id, access);
    let (ctx, peer) = (ctx.clone(), peer.clone());
    tokio::spawn(async move {
        let Some(c) = ctx.registry.channel(&id) else { return };
        c.members.write().unwrap_or_else(|e| e.into_inner()).insert(peer.device);
        let _ = ctx.hub.send(HubMsg::MemberSeen { folder: id, device: peer.device });
        let statements: Vec<Statement> = c.membership.read().unwrap_or_else(|e| e.into_inner()).statements().cloned().collect();
        let have = lock(&c.store).have(id);
        let _ = peer.control.send(Message::Members(Members { folder_id: id, statements })).await;
        let _ = peer.control.send(Message::ChatHave(have)).await;
        // Private messages go over the sessions of shared channels: the keys and what we hold.
        crate::direct::start(&ctx, &peer).await;
    })
}

/// A peer said what it holds: send what it lacks. A higher sequence number of this device's than we know (this
/// device lost its store) moves our counter past it.
pub(crate) async fn on_have(ctx: &Arc<Ctx>, peer: &Arc<PeerHandle>, c: &Arc<ChannelShared>, have: ChatHave) {
    let me = *ctx.key.public_key();
    if let Some(high) = have.authors.iter().find(|a| a.author == me).and_then(AuthorHave::highest) {
        let mut s = lock(&c.settings);
        s.seq = s.seq.max(high);
    }
    let missing = lock(&c.store).missing_for(&have);
    for part in batches(missing) {
        if peer.control.send(Message::ChatMessages(ChatMessages { channel_id: c.id, messages: part })).await.is_err() {
            return;
        }
    }
}

/// Messages from a peer (`from`): those whose author may post now are kept, told about and passed on.
pub(crate) async fn on_messages(ctx: &Arc<Ctx>, from: DeviceId, c: &Arc<ChannelShared>, msgs: Vec<ChatMessage>) {
    let received = now_ms();
    let mut new = Vec::new();
    for m in msgs {
        if !m.verify(&c.id) || !c.may_post(&m.author) {
            continue;
        }
        let kept = lock(&c.store).insert(m.clone(), received);
        match kept {
            Ok(true) => new.push(m),
            Ok(false) => {}
            Err(e) => c.problem(format!("the channel's messages could not be saved: {e}")),
        }
    }
    let mut names: HashMap<DeviceId, String> = ctx.registry.peers().iter().map(|p| (p.device, p.name.clone())).collect();
    names.insert(ctx.me, lock(&ctx.device_name).clone());
    announce(ctx, c, &new, Some(from), &names).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use trs_sync_proto::device::DeviceKey;

    fn msg(key: &DeviceKey, ch: &FolderId, seq: u64, at_ms: u64, body: &str) -> ChatMessage {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&seq.to_be_bytes());
        id[8] = key.public_key()[0];
        ChatMessage::sign(key, ch, Draft { id, seq, at_ms, reply_to: None, topic: None, agent: None, kind: MessageKind::Text, body: body.into() }).unwrap()
    }

    #[test]
    fn the_store_keeps_order_survives_a_restart_and_drops_the_oldest_over_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.chat");
        let (a, b) = (DeviceKey::from_seed(&[1; 32]), DeviceKey::from_seed(&[2; 32]));
        let ch = FolderId([9; 32]);
        let mut s = Store::open(p.clone(), (10, MAX_BYTES)).unwrap();
        assert!(s.insert(msg(&a, &ch, 1, 100, "one"), 100).unwrap());
        assert!(s.insert(msg(&b, &ch, 1, 90, "two"), 100).unwrap());
        assert!(!s.insert(msg(&b, &ch, 1, 90, "two"), 100).unwrap(), "known");
        // Far in the future: shown when it arrived.
        assert!(s.insert(msg(&a, &ch, 2, 10_000_000, "later"), 110).unwrap());
        let order: Vec<&str> = s.msgs.values().map(|m| m.msg.text().unwrap()).collect();
        assert_eq!(order, ["two", "one", "later"]);
        let have = s.have(ch);
        assert_eq!(have.authors.iter().map(|h| h.ranges.len()).sum::<usize>(), 2);
        drop(s);

        let mut s = Store::open(p.clone(), (10, MAX_BYTES)).unwrap();
        assert_eq!(s.msgs.len(), 3, "kept on disk");
        assert_eq!(s.highest(a.public_key()), 2);
        let theirs = ChatHave { channel_id: ch, floor_at_ms: 0, authors: vec![AuthorHave::of(*a.public_key(), [1])] };
        let missing: Vec<String> = s.missing_for(&theirs).iter().map(|m| m.text().unwrap().to_string()).collect();
        assert_eq!(missing, ["two", "later"]);
        for i in 3..20 {
            s.insert(msg(&a, &ch, i, 200 + i, "x"), 300).unwrap();
        }
        assert!(s.msgs.len() <= 10 && s.floor_at_ms > 0);
        assert!(!s.insert(msg(&b, &ch, 2, 1, "too old"), 300).unwrap(), "older than what is kept");
        let n = s.msgs.len();
        drop(s);
        // A cut-off tail is dropped, the rest kept.
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.extend_from_slice(&[40, 1, 2]);
        std::fs::write(&p, &bytes).unwrap();
        assert_eq!(Store::open(p.clone(), (10, MAX_BYTES)).unwrap().msgs.len(), n);
        std::fs::write(&p, [&b"SWCH\x02"[..]].concat()).unwrap();
        assert!(Store::open(p.clone(), (10, MAX_BYTES)).err().is_some_and(|e| e.to_string().contains("newer Seedwell")));
    }

    #[test]
    fn agent_access_names() {
        for a in [AgentAccess::Off, AgentAccess::Read, AgentAccess::ReadPost] {
            assert_eq!(AgentAccess::parse(a.name()), Some(a));
        }
        assert_eq!(AgentAccess::parse("read_post"), Some(AgentAccess::ReadPost));
        assert_eq!(AgentAccess::parse("all"), None);
        assert!(AgentAccess::Off < AgentAccess::Read && AgentAccess::Read < AgentAccess::ReadPost);
    }

    #[test]
    fn the_wake_up_command_reads_the_message_on_stdin_and_is_stopped_after_its_time() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let cmd = format!("cat > '{}'; echo \"$#\" >> '{}'", out.display(), out.display());
        let t = Duration::from_secs(5);
        run_wake(&cmd, &FolderId([1; 32]), "{\"body\":\"hi\"}", dir.path(), t).unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "{\"body\":\"hi\"}\n0\n", "on stdin, no arguments");
        assert!(run_wake("exit 3", &FolderId([1; 32]), "{}", dir.path(), t).is_err());
        // A big message to a command that exits without reading it: no SIGPIPE, no error from the input.
        let big = format!("{{\"body\":\"{}\"}}", "x".repeat(100_000));
        run_wake("true", &FolderId([1; 32]), &big, dir.path(), t).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "the input file is gone");
    }

    #[test]
    fn a_command_that_never_reads_is_stopped_and_the_next_message_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let s = trs_sync_proto::keys::FolderSecret::from_bytes([3; 32]);
        let id = FolderId::of_public_key(s.public_key());
        let space = Space::new(id, SpaceKind::Channel, FolderKey::Member(*s.public_key()), None, Membership::new(*s.public_key()), dir.path().join("m"), None, BTreeMap::new(), "x".into());
        let store = Store::open(dir.path().join("x.chat"), (10, MAX_BYTES)).unwrap();
        let c = ChannelShared { space: Arc::new(space), store: Mutex::new(store), settings: Mutex::new(Settings::default()), wake: Mutex::new(None), wake_dir: dir.path().to_path_buf() };
        // More than a pipe holds, to a command that never reads: stopped after its time.
        let started = std::time::Instant::now();
        wake(&c, "sleep 60", format!("{{\"body\":\"{}\"}}", "y".repeat(70 * 1024)));
        let out = dir.path().join("second");
        wake(&c, &format!("cat > '{}'", out.display()), "{\"body\":\"next\"}".into());
        while !out.exists() || std::fs::read_to_string(&out).unwrap().is_empty() {
            assert!(started.elapsed() < Duration::from_secs(20), "the second message never ran");
            std::thread::sleep(Duration::from_millis(50));
        }
        let took = started.elapsed();
        assert!(took >= Duration::from_secs(2) && took < Duration::from_secs(10), "{took:?}");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "{\"body\":\"next\"}\n");
    }
}
