//! The `chat.*` methods (SPEC §7G.7 *Channels*): channels through the control socket. A child of `server` so it
//! shares the permission gate and the activity log.
//!
//! Each channel has its own agent switch (off by default) on top of the agent permission level: `chat.read` (and
//! the `chat.message` events) need the channel at Read or above, `chat.send` needs it at Read and post. A message an
//! agent posts always carries an agent name, set here: the request's `agent` (its project's, say), else the client's
//! name from its `hello`, so no client can post as the user. Only the user's own posts on this device are
//! `"trust": "this-device"`; an agent's here is `untrusted-agent`, another device's `untrusted-peer`: data, never
//! instructions.
//!
//! Permissions: read-only for `chat.list`, `chat.members` and `chat.read`; control for `chat.new`, `chat.join`,
//! `chat.send`, `chat.mute` and `chat.leave`; full for what shares a channel or changes who may do what in it
//! (`chat.share`, `chat.set_role`, `chat.remove_device`, `chat.set_share_until`, `chat.set_agents`,
//! `chat.set_wake`).

use std::sync::Arc;

use serde_json::json;
use tokio::sync::{broadcast, watch};
use trs_sync_net::SyncHandle;
use trs_sync_net::chat::{AgentAccess, ChannelStatus, MessageKind};
use trs_sync_proto::ids::FolderId;

use super::{Conn, Ctx, Reply, reply};
use crate::backend::Backend;
use crate::proto::*;
use crate::sync::{self as s, channel_dto, chat_err, message_dto, resolve_channel};
use crate::types::*;

pub(super) fn pick(h: &SyncHandle, q: &str) -> Result<(ChannelStatus, FolderId), ApiError> {
    let all = h.chat_channels();
    let c = resolve_channel(&all, q)?.clone();
    let id = c.id.parse().map_err(|_| ApiError::internal("the service returned a malformed channel id"))?;
    Ok((c, id))
}

fn channel_reply(h: &SyncHandle, id: &FolderId, what: &str) -> Result<Reply, ApiError> {
    let c = h.channel_status(id).ok_or_else(|| ApiError::not_found("the channel is no longer on this Mac"))?;
    let d = channel_dto(&c);
    let mut r = reply(&d)?;
    r.note = Some(format!("{what} {} ({})", clip(&d.name, 80), short(&d.id)));
    Ok(r)
}

/// The channel's agent switch allows `need`.
fn agents_allow(c: &ChannelStatus, need: AgentAccess, what: &str) -> Result<(), ApiError> {
    if c.agents >= need {
        return Ok(());
    }
    let (how, choice) = if need == AgentAccess::ReadPost { ("read and post", "Read and post") } else { ("read", "Read (or Read and post)") };
    let name = clip(&c.name, 60);
    Err(ApiError::new(
        ErrorCode::PermissionDenied,
        format!(
            "agents on this Mac may not {how} in the channel `{name}` yet; the user can allow it in Seedwell: Chats ▸ {name} ▸ ⋯ ▸ Agents on This Mac ▸ {choice}. Settings › Agents does not open up chats: each channel has its own switch"
        ),
    )
    .with_data(json!({ "channel": c.id, "agents": c.agents.name(), "method": what })))
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
            "agents on this Mac may not {how} private messages yet; the user can allow it in Seedwell: Chats ▸ a private conversation ▸ ⋯ ▸ Agents on This Mac ▸ {choice} (one switch for every private conversation). Settings › Agents does not open up chats"
        ),
    )
    .with_data(json!({ "agents": h.chat_direct_agents().name(), "method": what })))
}

/// A device named by a caller: its id, a prefix of it, or its name, among the conversations and the members of
/// this Mac's channels.
fn pick_device(h: &SyncHandle, q: &str) -> Result<trs_sync_proto::ids::DeviceId, ApiError> {
    let mut members: Vec<trs_sync_net::MemberInfo> = Vec::new();
    for c in h.chat_channels() {
        if let Ok(id) = c.id.parse() {
            members.extend(h.members(&id).unwrap_or_default().into_iter().filter(|m| !m.is_me));
        }
    }
    for d in h.chat_directs() {
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
        Request::ChatList(_) => reply(&ChatListDto { channels: h.chat_channels().iter().map(channel_dto).collect() }),
        Request::ChatNew(p) => {
            let c = h.chat_new(&p.name).await.map_err(chat_err)?;
            channel_reply(&h, &c.id.parse().map_err(|_| ApiError::internal("bad channel id"))?, "made the channel")
        }
        Request::ChatJoin(p) => {
            let c = h.chat_join(&p.link).await.map_err(chat_err)?;
            channel_reply(&h, &c.id.parse().map_err(|_| ApiError::internal("bad channel id"))?, "joined the channel")
        }
        Request::ChatShare(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            let role = s::parse_role(p.role.as_deref().unwrap_or("read_write"))?;
            let inv = h.create_invite(&id, role, p.uses.unwrap_or(1), p.days.unwrap_or(7)).map_err(chat_err)?;
            let mut r = reply(&s::invite_dto(&inv))?;
            let uses = if inv.max_uses == 0 { "unlimited uses".to_string() } else { format!("{} use(s)", inv.max_uses) };
            r.note = Some(format!("a {} link ({uses}) to channel {} ({}) made", s::role_name(role), clip(&c.name, 80), short(&c.id)));
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
        Request::ChatSend(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            agents_allow(&c, AgentAccess::ReadPost, "chat.send")?;
            let agent = agent_for(conn, p.agent.as_deref())?;
            let kind = if p.json { MessageKind::Json } else { MessageKind::Text };
            let m = h.chat_send(&id, p.topic.as_deref(), kind, &p.body, p.reply_to.as_deref(), Some(&agent)).await.map_err(chat_err)?;
            let mut r = reply(&message_dto(&m))?;
            r.note = Some(format!("posted {} to channel {} ({})", short(&m.id), clip(&c.name, 80), short(&c.id)));
            Ok(r)
        }
        Request::ChatRead(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            agents_allow(&c, AgentAccess::Read, "chat.read")?;
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
            agents_allow(&c, AgentAccess::Read, "chat.topics")?;
            let topics = h.chat_topics(&id).map_err(chat_err)?;
            reply(&ChatTopicsDto { id: c.id, name: c.name, topics: topics.iter().map(s::topic_dto).collect() })
        }
        Request::ChatNewTopic(p) => {
            let (c, id) = pick(&h, &p.channel)?;
            agents_allow(&c, AgentAccess::ReadPost, "chat.new_topic")?;
            let t = h.chat_new_topic(&id, &p.title, Some(&agent_for(conn, p.agent.as_deref())?)).await.map_err(chat_err)?;
            let mut r = reply(&s::topic_dto(&t))?;
            r.note = Some(format!("made topic {} in channel {} ({})", clip(&t.title, 80), clip(&c.name, 80), short(&c.id)));
            Ok(r)
        }
        Request::ChatDmList(_) => {
            direct_allow(&h, AgentAccess::Read, "chat.dm_list")?;
            reply(&ChatDirectsDto { agents: h.chat_direct_agents().name().into(), conversations: h.chat_directs().iter().map(s::direct_dto).collect() })
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
            Ok(r)
        }
        Request::ChatDmDelete(p) => {
            let dev = pick_device(&h, &p.device)?;
            h.chat_delete_direct(&dev).map_err(|e| match e {
                trs_sync_net::SyncError::NotFound => ApiError::not_found("no private conversation with that device (see `seedwell chat dm list`)"),
                e => chat_err(e),
            })?;
            let mut r = reply(&json!({ "deleted": true, "device_id": dev.to_string() }))?;
            r.note = Some(format!("deleted the private conversation with {}", short(&dev.to_string())));
            Ok(r)
        }
        Request::ChatDmSetAgents(p) => {
            let a = AgentAccess::parse(&p.agents).ok_or_else(|| ApiError::params("`agents` is off, read or post"))?;
            h.chat_set_direct_agents(a).map_err(chat_err)?;
            reply(&json!({ "agents": a.name() }))
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
        Request::ChatSetAgents(p) => {
            let (_, id) = pick(&h, &p.channel)?;
            let a = AgentAccess::parse(&p.agents).ok_or_else(|| ApiError::params("`agents` is off, read or post"))?;
            h.chat_set_agents(&id, a).map_err(chat_err)?;
            channel_reply(&h, &id, "agent access changed for")
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
            Ok(r)
        }
        _ => Err(ApiError::internal("not a chat method")),
    }
}

/// Pumps new channel messages into the event stream as `chat.message` events: only those of channels whose agent
/// switch is at Read or above (private messages: the Mac's switch for them). Waits for the sync service, and follows it when it starts again.
pub(super) async fn pump<B: Backend>(ctx: Arc<Ctx<B>>, mut stop: watch::Receiver<bool>) {
    let mut rx: Option<broadcast::Receiver<trs_sync_net::chat::ChatEvent>> = None;
    loop {
        let Some(r) = rx.as_mut() else {
            rx = ctx.backend.sync().map(|h| h.chat_subscribe());
            if rx.is_none() {
                // Not running yet (or again): a cheap look, often, so the first messages are not missed.
                tokio::select! {
                    _ = stop.changed() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            }
            continue;
        };
        tokio::select! {
            _ = stop.changed() => return,
            e = r.recv() => match e {
                Ok(e) if e.agents >= AgentAccess::Read => ctx.hub.publish_with(
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
                Err(broadcast::error::RecvError::Lagged(n)) => ctx.hub.publish(EventDto {
                    kind: "lagged".into(),
                    data: json!({ "missed": n, "what": "chat messages" }),
                    ..Default::default()
                }),
                Err(broadcast::error::RecvError::Closed) => rx = None,
            },
        }
    }
}
