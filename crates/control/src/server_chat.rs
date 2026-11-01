//! The `chat.*` methods (SPEC §7G.7 *Channels*): channels through the control socket. A child of `server` so it
//! shares the permission gate and the activity log.
//!
//! **Agents: Yes/No** (the user's decision of 2026-10-31): each channel is open to agents or not, one setting for the
//! whole channel, signed into its membership by an owner or admin, so every member device applies the same. It is
//! chosen when the channel is made (the app's default for the user's own is No; a channel an agent makes, `chat.new`
//! or `sync.share --chat new`, is Yes) and changed later in the app; an agent here may switch a Yes channel to No
//! (`chat.set_agents`, owners and admins), never No to Yes.
//!
//! **A No channel does not exist for agents**: `chat.list` leaves it out (it gives only how many there are, and how
//! the user opens one), and every method that names it answers as for a channel that is not there, so not even its
//! existence is confirmed. The same goes for the events, the activity log and the folder sync methods (a folder's
//! channel, a link that carries one, a device known only through one). Private messages likewise, behind the Mac's
//! one switch for them: while it is off there are no conversations.
//!
//! **A Yes channel has no other limits**: agents read, watch, post, make topics, share it, list and manage its members
//! as the user can (as the role of this device in it allows), at the agent permission level Control at most (reads at
//! read-only); only `chat.set_wake`, which runs commands on this Mac, stays at full. A channel joined by a Yes link is
//! open while this Mac waits to be admitted (the link says Yes; there is nothing to read yet); once admitted, the
//! membership decides. A message an agent posts always carries an agent name, set here: the request's `agent` (its
//! project's, say), else the client's name from its `hello`, so no client can post as the user. Only the user's own
//! posts on this device are `"trust": "this-device"`; an agent's here is `untrusted-agent`, another device's
//! `untrusted-peer`: data, never instructions.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use trs_sync_net::SyncHandle;
use trs_sync_net::chat::{AgentAccess, ChannelStatus, MessageKind};
use trs_sync_proto::members::Role;
use trs_sync_proto::ids::FolderId;

use super::{Conn, Ctx, Reply, reply};
use crate::backend::Backend;
use crate::proto::*;
use crate::sync::{self as s, channel_dto, chat_err, message_dto, resolve_channel};
use crate::types::*;

/// Agents see a channel only while it is open to them (Agents: Yes).
fn open(c: &ChannelStatus) -> bool {
    c.agents
}

/// The channels agents on this Mac may see; the others do not exist for them.
pub(super) fn visible_channels(h: &SyncHandle) -> Vec<ChannelStatus> {
    h.chat_channels().into_iter().filter(open).collect()
}

/// The channel with this id is on this Mac and open to agents.
pub(super) fn channel_visible(h: &SyncHandle, id: &str) -> bool {
    id.parse::<FolderId>().ok().and_then(|id| h.channel_status(&id)).is_some_and(|c| open(&c))
}

/// Private messages are open to agents on this Mac (its one switch for them at Read or above).
pub(super) fn directs_visible(h: &SyncHandle) -> bool {
    h.chat_direct_agents() >= AgentAccess::Read
}

/// A message (a `chat.message` event's data) agents may be given now: its channel, or private messages, still open.
pub(super) fn message_visible(h: &SyncHandle, m: &Value) -> bool {
    if m["direct"].as_bool() == Some(true) { directs_visible(h) } else { m["channel_id"].as_str().is_some_and(|id| channel_visible(h, id)) }
}

/// The channel `q` names, among those open to agents: a closed one is answered as one that is not there.
pub(super) fn pick(h: &SyncHandle, q: &str) -> Result<(ChannelStatus, FolderId), ApiError> {
    let all = visible_channels(h);
    let c = resolve_channel(&all, q)?.clone();
    let id = c.id.parse().map_err(|_| ApiError::internal("the service returned a malformed channel id"))?;
    Ok((c, id))
}

fn channel_reply(h: &SyncHandle, id: &FolderId, what: &str) -> Result<Reply, ApiError> {
    let c = h.channel_status(id).filter(open).ok_or_else(|| ApiError::not_found(concat!("the channel is no longer on this ", trs_platform::computer!())))?;
    let d = channel_dto(&c);
    let mut r = reply(&d)?;
    r.note = Some(format!("{what} {} ({})", clip(&d.name, 80), short(&d.id)));
    r.channel = Some(d.id);
    Ok(r)
}

/// How a channel's owner or admin opens it to agents, as the app names it.
fn how_to_open(name: &str) -> String {
    format!("its owner or an admin sets Agents: Yes in Seedwell: Chats ▸ {name} ▸ ⋯ ▸ Agents")
}

/// A channel an agent just joined that is closed to agents (Agents: No, or a link that does not say Yes): only what
/// the agent gave (the link's name) and its id, nothing of the channel itself.
fn closed_reply(id: &str, name: &str, what: &str) -> Result<Reply, ApiError> {
    let d = ChatChannelDto {
        id: id.to_owned(),
        name: clip(&crate::ops::clean_name(name), 200),
        joining: true,
        agents: "no".into(),
        ..Default::default()
    };
    let mut r = reply(&d)?;
    r.note = Some(format!("{what} {} ({}), closed to agents (Agents: No)", clip(&d.name, 80), short(&d.id)));
    r.channel = Some(d.id);
    Ok(r)
}

/// The name a channel joined with `l` gets on this Mac, as the service names it: the link's, else "Channel".
fn link_name(l: &trs_sync_proto::members::InviteLink) -> String {
    let n: String = l.name.as_deref().unwrap_or_default().chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(100).collect();
    if n.is_empty() { "Channel".into() } else { n }
}

/// The label of what this connection posts when the call names none: its client name from `hello`.
fn agent_of(conn: &Conn) -> String {
    let agent = conn.client.as_deref().map(|n| n.chars().filter(|c| !c.is_control()).take(trs_sync_proto::chat::MAX_AGENT).collect::<String>());
    agent.filter(|a| !a.trim().is_empty()).unwrap_or_else(|| "agent".into())
}

/// The agent name a post goes under: the one the request gives (its project's, say), else the client's.
fn agent_for(conn: &Conn, given: Option<&str>) -> Result<String, ApiError> {
    let Some(given) = given else { return Ok(agent_of(conn)) };
    let name: String = given.chars().filter(|c| !c.is_control()).collect();
    let name = name.trim();
    let n = name.chars().count();
    if n == 0 || n > trs_sync_proto::chat::MAX_AGENT {
        return Err(ApiError::params(format!("`agent` is a name of 1 to {} characters", trs_sync_proto::chat::MAX_AGENT)));
    }
    Ok(name.to_owned())
}

/// The Mac's switch for private messages allows `need`.
fn direct_allow(h: &SyncHandle, need: AgentAccess, what: &str) -> Result<(), ApiError> {
    if h.chat_direct_agents() >= need {
        return Ok(());
    }
    let (how, choice) = if need == AgentAccess::ReadPost { ("read and post", "Read and post") } else { ("read", "Read (or Read and post)") };
    Err(ApiError::new(
        ErrorCode::PermissionDenied,
        format!(
            concat!("agents on this ", trs_platform::computer!(), " may not {how} private messages yet; the user can allow it in Seedwell: Chats ▸ a private conversation ▸ ⋯ ▸ Agents on This ", trs_platform::computer!(), " ▸ {choice} (one switch for every private conversation). Settings › Agents does not open up chats"),
            how = how,
            choice = choice
        ),
    )
    .with_data(json!({ "agents": h.chat_direct_agents().name(), "method": what })))
}

/// A device named by a caller: its id, a prefix of it, or its name, among the members of the channels open to
/// agents and, while private messages are open to them, the conversations.
fn pick_device(h: &SyncHandle, q: &str) -> Result<trs_sync_proto::ids::DeviceId, ApiError> {
    let mut members: Vec<trs_sync_net::MemberInfo> = Vec::new();
    for c in visible_channels(h) {
        if let Ok(id) = c.id.parse() {
            members.extend(h.members(&id).unwrap_or_default().into_iter().filter(|m| !m.is_me));
        }
    }
    let directs = if directs_visible(h) { h.chat_directs() } else { Vec::new() };
    for d in directs {
        if !members.iter().any(|m| m.device == d.device) {
            members.push(trs_sync_net::MemberInfo { device: d.device, name: d.name, user: d.user, role: trs_sync_proto::members::Role::ReadOnly, online: d.online, is_me: false, expires_s: None });
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    members.retain(|m| seen.insert(m.device));
    Ok(s::resolve_member(&members, q)?.device)
}

/// Run one `chat.*` request.
pub(super) async fn call<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, req: Request) -> Result<Reply, ApiError> {
    let h = ctx.backend.sync().ok_or_else(s::not_running)?;
    match req {
        Request::ChatList(_) => {
            let all = h.chat_channels();
            let hidden = all.iter().filter(|c| !open(c)).count() as u32;
            let hint = (hidden > 0).then(|| {
                format!(
                    concat!("{} on this ", trs_platform::computer!(), " {} closed to agents; {}"),
                    if hidden == 1 { "1 channel".to_owned() } else { format!("{hidden} channels") },
                    if hidden == 1 { "is" } else { "are" },
                    how_to_open("<channel>")
                )
            });
            reply(&ChatListDto { channels: all.iter().filter(|c| open(c)).map(channel_dto).collect(), hidden, hint })
        }
        Request::ChatNew(p) => {
            // An agent's channel is Agents: Yes: open to agents, here and on every device that joins it.
            let c = h.chat_new(&p.name, true).await.map_err(chat_err)?;
            let id = c.id.parse().map_err(|_| ApiError::internal("bad channel id"))?;
            channel_reply(&h, &id, "made the channel")
        }
        Request::ChatJoin(p) => {
            let link = trs_sync_proto::members::InviteLink::parse_chat(&p.link).ok();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            // A channel already here but closed to agents: answered as the join of a closed one, so a link does not
            // tell an agent what the channel holds.
            if let Some(l) = link.as_ref().filter(|l| !l.invite.expired(now))
                && h.channel_status(&l.invite.folder_id()).is_some_and(|c| !open(&c))
            {
                return closed_reply(&l.invite.folder_id().to_string(), &link_name(l), "joined the channel");
            }
            let c = h.chat_join(&p.link).await.map_err(chat_err)?;
            let id = c.id.parse().map_err(|_| ApiError::internal("bad channel id"))?;
            match link.as_ref() {
                Some(l) if !open(&c) => closed_reply(&c.id, &link_name(l), "joined the channel"),
                _ => channel_reply(&h, &id, "joined the channel"),
            }
        }
        Request::ChatShare(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let role = s::parse_role(p.role.as_deref().unwrap_or("read_write"))?;
            let inv = h.create_invite(&id, role, p.uses.unwrap_or(1), p.days.unwrap_or(7)).map_err(chat_err)?;
            let mut r = reply(&s::invite_dto(&inv))?;
            let uses = if inv.max_uses == 0 { "unlimited uses".to_string() } else { format!("{} use(s)", inv.max_uses) };
            r.note = Some(format!("a {} link ({uses}) to channel {} ({}) made", s::role_name(role), clip(&c.name, 80), short(&c.id)));
            r.channel = Some(c.id);
            Ok(r)
        }
        Request::ChatMembers(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let members = h.members(&id).map_err(chat_err)?;
            reply(&SyncMembersDto { id: c.id, name: c.name, members: members.iter().map(s::member_dto).collect() })
        }
        Request::ChatSetRole(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let role = s::parse_role(&p.role)?;
            let members = h.members(&id).map_err(chat_err)?;
            let m = s::resolve_member(&members, &p.device)?;
            h.set_role(&id, &m.device, role).map_err(chat_err)?;
            let mut r = channel_reply(&h, &id, "role changed in")?;
            r.note = Some(format!("{} is now {} in channel {}", clip(&m.name, 60), s::role_name(role), clip(&c.name, 80)));
            Ok(r)
        }
        Request::ChatRemoveDevice(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let members = h.members(&id).map_err(chat_err)?;
            let m = s::resolve_member(&members, &p.device)?;
            h.remove_device(&id, &m.device).map_err(chat_err)?;
            let mut r = channel_reply(&h, &id, "device removed from")?;
            r.note = Some(format!("{} removed from channel {}", clip(&m.name, 60), clip(&c.name, 80)));
            Ok(r)
        }
        Request::ChatSetShareUntil(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let span = u64::from(p.days) * 86_400;
            let until = match (p.days, p.extend, c.share_until) {
                (0, _, _) => 0,
                (_, true, Some(end)) => end.max(now) + span,
                (_, true, None) => return Err(ApiError::invalid("the channel is shared with no end; give the days from now instead")),
                (_, false, _) => now + span,
            };
            h.set_share_until(&id, until).map_err(chat_err)?;
            channel_reply(&h, &id, "sharing changed for")
        }
        Request::ChatSetAgents(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let open = match p.agents.as_str() {
                "yes" => true,
                "no" => false,
                _ => return Err(ApiError::params("`agents` is `yes` or `no`")),
            };
            if !c.role.is_some_and(Role::admits) {
                return Err(ApiError::new(ErrorCode::PermissionDenied, "only the channel's owners and admins can change whether agents may use it"));
            }
            h.chat_set_agents(&id, open).map_err(chat_err)?;
            if open {
                return channel_reply(&h, &id, "agents may use");
            }
            // Closed now: it does not exist for agents any more, so only what the agent named.
            let d = ChatChannelDto { id: c.id.clone(), name: c.name.clone(), agents: "no".into(), ..Default::default() };
            let mut r = reply(&d)?;
            r.note = Some(format!("closed channel {} ({}) to agents on every device (Agents: No)", clip(&c.name, 80), short(&c.id)));
            r.channel = Some(c.id);
            Ok(r)
        }
        Request::ChatSend(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let agent = agent_for(conn, p.agent.as_deref())?;
            let kind = if p.json { MessageKind::Json } else { MessageKind::Text };
            let m = h.chat_send(&id, p.topic.as_deref(), kind, &p.body, p.reply_to.as_deref(), Some(&agent)).await.map_err(chat_err)?;
            let mut r = reply(&message_dto(&m))?;
            r.note = Some(format!("posted {} to channel {} ({})", short(&m.id), clip(&c.name, 80), short(&c.id)));
            r.channel = Some(c.id);
            Ok(r)
        }
        Request::ChatRead(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let msgs = match &p.unread_for {
                Some(a) => {
                    if p.since.is_some() {
                        return Err(ApiError::params("`unread_for` takes `last`, not `since`"));
                    }
                    let agent = agent_for(conn, Some(a))?;
                    h.chat_read_unread(&id, p.topic.as_deref().map(Some), &agent, p.last.map_or(200, |n| n.clamp(1, 1000)) as usize, p.peek).map_err(chat_err)?
                }
                None => {
                    let last = p.last.map(|n| n.clamp(1, 1000) as usize).or(if p.since.is_none() { Some(50) } else { None });
                    h.chat_read(&id, p.topic.as_deref().map(Some), p.since.as_deref(), last).map_err(chat_err)?
                }
            };
            reply(&ChatMessagesDto { id: c.id, name: c.name, messages: msgs.iter().take(1000).map(message_dto).collect() })
        }
        Request::ChatTopics(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let topics = h.chat_topics(&id).map_err(chat_err)?;
            reply(&ChatTopicsDto { id: c.id, name: c.name, topics: topics.iter().map(s::topic_dto).collect() })
        }
        Request::ChatNewTopic(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let t = h.chat_new_topic(&id, &p.title, Some(&agent_for(conn, p.agent.as_deref())?)).await.map_err(chat_err)?;
            let mut r = reply(&s::topic_dto(&t))?;
            r.note = Some(format!("made topic {} in channel {} ({})", clip(&t.title, 80), clip(&c.name, 80), short(&c.id)));
            r.channel = Some(c.id);
            Ok(r)
        }
        // While private messages are closed to agents there are none: no conversation to list, read or delete.
        Request::ChatDmList(_) if !directs_visible(&h) => reply(&ChatDirectsDto {
            agents: AgentAccess::Off.name().into(),
            conversations: Vec::new(),
            hint: Some(
                concat!(
                    "private messages are closed to agents on this ",
                    trs_platform::computer!(),
                    "; the user opens them in Seedwell: Chats ▸ a private conversation ▸ ⋯ ▸ Agents on This ",
                    trs_platform::computer!(),
                    " ▸ Read (or Read and post)"
                )
                .into(),
            ),
        }),
        Request::ChatDmList(_) => {
            direct_allow(&h, AgentAccess::Read, "chat.dm_list")?;
            reply(&ChatDirectsDto { agents: h.chat_direct_agents().name().into(), conversations: h.chat_directs().iter().map(s::direct_dto).collect(), hint: None })
        }
        Request::ChatDmRead(p) if !directs_visible(&h) => {
            let dev = pick_device(&h, &p.device)?;
            reply(&ChatMessagesDto { id: dev.to_string(), name: String::new(), messages: Vec::new() })
        }
        Request::ChatDmRead(p) => {
            direct_allow(&h, AgentAccess::Read, "chat.dm_read")?;
            let dev = pick_device(&h, &p.device)?;
            let msgs = h.chat_direct_messages(&dev, p.before.as_deref(), p.last.map_or(50, |n| n.clamp(1, 1000)) as usize).map_err(chat_err)?;
            let name = h.chat_directs().into_iter().find(|d| d.device == dev).map(|d| d.name).unwrap_or_default();
            reply(&ChatMessagesDto { id: dev.to_string(), name, messages: msgs.iter().map(message_dto).collect() })
        }
        Request::ChatDmSend(p) => {
            direct_allow(&h, AgentAccess::ReadPost, "chat.dm_send")?;
            let dev = pick_device(&h, &p.device)?;
            let agent = agent_for(conn, p.agent.as_deref())?;
            let kind = if p.json { MessageKind::Json } else { MessageKind::Text };
            let m = h.chat_send_direct(&dev, kind, &p.body, p.reply_to.as_deref(), Some(&agent)).await.map_err(chat_err)?;
            let mut r = reply(&message_dto(&m))?;
            r.note = Some(format!("wrote {} privately to {}", short(&m.id), short(&dev.to_string())));
            r.direct = true;
            Ok(r)
        }
        Request::ChatDmDelete(p) => {
            let none = || ApiError::not_found("no private conversation with that device (see `seedwell chat dm list`)");
            let dev = pick_device(&h, &p.device)?;
            if !directs_visible(&h) {
                return Err(none());
            }
            h.chat_delete_direct(&dev).map_err(|e| match e {
                trs_sync_net::SyncError::NotFound => none(),
                e => chat_err(e),
            })?;
            let mut r = reply(&json!({ "deleted": true, "device_id": dev.to_string() }))?;
            r.note = Some(format!("deleted the private conversation with {}", short(&dev.to_string())));
            r.direct = true;
            Ok(r)
        }
        Request::ChatAddAddress(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let a = super::sync_methods::address(&p.address).await?;
            h.add_address(&id, a).map_err(chat_err)?;
            let mut r = channel_reply(&h, &id, "address added to")?;
            r.note = Some(format!("{a} for channel {} ({})", clip(&c.name, 80), short(&c.id)));
            Ok(r)
        }
        Request::ChatRename(p) => {
            let (_, id) = pick(&h, &p.channel)?;
            h.chat_rename(&id, &p.name).map_err(chat_err)?;
            channel_reply(&h, &id, "renamed")
        }
        Request::ChatMute(p) => {
            let (_, id) = pick(&h, &p.channel)?;
            h.chat_set_muted(&id, p.muted).map_err(chat_err)?;
            channel_reply(&h, &id, if p.muted { "muted" } else { "unmuted" })
        }
        Request::ChatSetWake(p) => {
            let (_, id) = pick(&h, &p.channel)?;
            h.chat_set_wake_command(&id, p.command.as_deref()).map_err(chat_err)?;
            channel_reply(&h, &id, "wake-up command changed for")
        }
        Request::ChatLeave(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            h.chat_leave(&id).map_err(chat_err)?;
            let mut r = reply(&json!({ "left": true, "id": c.id, "name": c.name }))?;
            r.note = Some(format!("left channel {} ({})", clip(&c.name, 80), short(&c.id)));
            r.channel = Some(c.id);
            Ok(r)
        }
        _ => Err(ApiError::internal("not a chat method")),
    }
}

type ChatRx = broadcast::Receiver<trs_sync_net::chat::ChatEvent>;

/// Hands the pump a receiver of the running sync service's messages, made now: what the service sends from here on
/// reaches the event stream, though the pump looks for the service on its own only now and then. `events_subscribe`
/// and `events` call it before they answer, so an agent misses no message posted after (one posted right after the
/// service started, or started again, was lost until the pump's next look).
pub(super) fn tap<B: Backend>(ctx: &Ctx<B>) {
    if let Some(h) = ctx.backend.sync() {
        let _ = ctx.chat_taps.send(h.chat_subscribe());
    }
}

/// How often the pump looks for the sync service: often while there is none, seldom while one runs (one started again
/// in its place is followed then, or at the next [`tap`]).
const LOOK_IDLE: std::time::Duration = std::time::Duration::from_millis(100);
const LOOK_RUNNING: std::time::Duration = std::time::Duration::from_secs(1);

/// Keeps `new` as the pump's receiver unless the one it has listens to the same service (it then has every message
/// `new` would give, and maybe some before).
fn adopt(rx: &mut Option<ChatRx>, new: Option<ChatRx>) {
    match (rx.as_ref(), new) {
        (Some(old), Some(new)) if old.same_channel(&new) => {}
        (_, new) => *rx = new,
    }
}

/// Pumps new channel messages into the event stream as `chat.message` events: only those of channels open to agents
/// (private messages: the Mac's switch for them); the server checks again whenever it hands
/// one out (`event_visible`). Follows the sync service as it starts, stops and starts again: through the receivers
/// [`tap`] hands it, and by looking for the service now and then.
pub(super) async fn pump<B: Backend>(ctx: Arc<Ctx<B>>, mut taps: tokio::sync::mpsc::UnboundedReceiver<ChatRx>, mut stop: watch::Receiver<bool>) {
    let mut rx: Option<ChatRx> = None;
    let mut look = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            Some(new) = taps.recv() => {
                // One of a service that stopped since (the pump may have found the next one already) is of no use.
                if ctx.backend.sync().is_some_and(|h| h.chat_subscribe().same_channel(&new)) {
                    adopt(&mut rx, Some(new));
                }
            }
            _ = tokio::time::sleep_until(look) => {
                adopt(&mut rx, ctx.backend.sync().map(|h| h.chat_subscribe()));
                look = tokio::time::Instant::now() + if rx.is_some() { LOOK_RUNNING } else { LOOK_IDLE };
            }
            Some(e) = async { Some(rx.as_mut()?.recv().await) } => match e {
                Ok(e) if e.agents => ctx.hub.publish_with(
                    EventDto {
                        kind: "chat.message".into(),
                        // A private message: the other device, so a stream can be filtered by conversation.
                        id: Some(e.message.peer_device.clone().unwrap_or_else(|| e.message.channel_id.clone())),
                        name: Some(if e.message.direct { e.message.author_name.clone() } else { e.message.channel_name.clone() }),
                        data: serde_json::to_value(message_dto(&e.message)).unwrap_or_default(),
                        ..Default::default()
                    },
                    false,
                ),
                Ok(_) => {}
                // Not how many: the count would take in the channels closed to agents.
                Err(broadcast::error::RecvError::Lagged(_)) => ctx.hub.publish(EventDto {
                    kind: "lagged".into(),
                    data: json!({ "what": "chat messages" }),
                    ..Default::default()
                }),
                // The service stopped: look for the next one, often.
                Err(broadcast::error::RecvError::Closed) => {
                    rx = None;
                    look = tokio::time::Instant::now() + LOOK_IDLE;
                }
            },
        }
    }
}
