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

/// What `chat.send` and `chat.dm_send` attach: images given as bytes, and files on this computer by their full paths.
fn attachments_of(uploads: &[crate::proto::ChatAttachmentUpload], files: &[String], json: bool) -> Result<Vec<trs_sync_net::chat::NewAttachment>, ApiError> {
    use trs_sync_net::chat::{NewAttachment, NewImage};
    if uploads.is_empty() && files.is_empty() {
        return Ok(Vec::new());
    }
    if json {
        return Err(ApiError::params("attachments go with a text message, not `json`"));
    }
    let mut out = Vec::with_capacity(uploads.len() + files.len());
    for a in uploads {
        let bytes = crate::b64::decode(&a.base64, trs_media::MAX_IMAGE)
            .ok_or_else(|| ApiError::params(format!("`{}`: not valid base64, or larger than {}", clip(&a.name, 100), trs_media::size_text(trs_media::MAX_IMAGE as u64))))?;
        out.push(NewAttachment::Image(NewImage { name: a.name.clone(), bytes }));
    }
    for f in files {
        if !std::path::Path::new(f).is_absolute() {
            return Err(ApiError::params(format!("`{}`: give the file's full path", clip(f, 200))));
        }
        out.push(NewAttachment::Path(f.into()));
    }
    Ok(out)
}

/// Where a call about a message's attachment goes: a channel open to agents, or (`device`) the private conversation
/// with a device while the Mac lets agents read private messages. Its place, the channel's id, and its name.
fn place_for(h: &SyncHandle, channel: &str, device: Option<&str>, what: &str) -> Result<(trs_sync_net::room::Place, Option<String>, String), ApiError> {
    match device {
        Some(d) => {
            direct_allow(h, AgentAccess::Read, what)?;
            let dev = pick_device(h, d)?;
            let name = h.chat_directs().into_iter().find(|x| x.device == dev).map(|x| x.name).filter(|n| !n.is_empty()).unwrap_or_else(|| short(&dev.to_string()).to_string());
            Ok((trs_sync_net::room::Place::Direct(dev), None, format!("the private conversation with {name}")))
        }
        None => {
            let (c, id) = pick(h, channel)?;
            Ok((trs_sync_net::room::Place::Channel(id), Some(c.id), format!("channel {}", c.name)))
        }
    }
}

/// What a file's state says, for people.
fn file_note(a: &crate::types::ChatAttachmentDto, place: &str) -> String {
    let size = trs_media::size_text(a.size);
    // Not its name, nor where it is: the note goes into the activity log.
    let what = format!("file {} ({size}) of {}", short(&a.hash), clip(place, 80));
    match a.state.as_str() {
        "downloading" => format!("{what}: downloading, {} of {size}", trs_media::size_text(a.done)),
        "paused" => format!("{what}: paused, {} of {size}; it resumes when a device that has it is connected", trs_media::size_text(a.done)),
        "downloaded" => format!("{what}: downloaded (`path` says where)"),
        "shared" => format!("{what}: shared from this computer (`path` says where)"),
        "available" => format!("{what}: not downloaded (chat.download gets it)"),
        s => format!("{what}: {}{}", s.replace('_', " "), a.problem.as_deref().map(|p| format!(" ({})", clip(p, 200))).unwrap_or_default()),
    }
}

/// The answer to a file call: where the file stands.
fn file_reply(place: trs_sync_net::room::Place, cid: Option<String>, name: &str, info: &trs_sync_net::chat::AttachmentInfo, direct: bool) -> Result<Reply, ApiError> {
    let d = ChatAttachmentDataDto { channel_id: place.to_string(), attachment: s::attachment_dto(info), ..Default::default() };
    let mut r = reply(&d)?;
    r.note = Some(file_note(&d.attachment, name));
    r.channel = cid;
    r.direct = direct;
    Ok(r)
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
    format!(
        "its owner or an admin sets Agents: Yes in Seedwell: Chats ▸ {name} ▸ ⋯ ▸ Agents; ask the user with `seedwell agents request channel-agents {name} --why \"<what for>\"`"
    )
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
    let (how, choice, ask) =
        if need == AgentAccess::ReadPost { ("read and post", "Read and post", "dm-agents post") } else { ("read", "Read (or Read and post)", "dm-agents") };
    Err(ApiError::new(
        ErrorCode::PermissionDenied,
        format!(
            concat!("agents on this ", trs_platform::computer!(), " may not {how} private messages yet; the user can allow it in Seedwell: Chats ▸ a private conversation ▸ ⋯ ▸ Agents on This ", trs_platform::computer!(), " ▸ {choice} (one switch for every private conversation; Settings › Agents does not open up chats). Ask the user with `seedwell agents request {ask} --why \"<what for>\"`"),
            how = how,
            choice = choice,
            ask = ask
        ),
    )
    .with_data(json!({ "agents": h.chat_direct_agents().name(), "method": what, "ask": format!("seedwell agents request {ask}") })))
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
            let items = attachments_of(&p.attachments, &p.files, p.json)?;
            let m = if items.is_empty() {
                h.chat_send(&id, p.topic.as_deref(), kind, &p.body, p.reply_to.as_deref(), Some(&agent)).await.map_err(chat_err)?
            } else {
                h.chat_send_attachments(id, p.topic.as_deref(), &p.body, p.reply_to.as_deref(), Some(&agent), items).await.map_err(chat_err)?
            };
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
        Request::ChatAttachment(p) => {
            let (place, cid, name) = place_for(&h, &p.channel, p.device.as_deref(), "chat.attachment")?;
            let (info, bytes) = h.chat_attachment(place, &p.hash).map_err(chat_err)?;
            let mut d = ChatAttachmentDataDto { channel_id: place.to_string(), attachment: s::attachment_dto(&info), ..Default::default() };
            if info.is_file() {
                let mut r = reply(&d)?;
                r.note = Some(file_note(&d.attachment, &name));
                r.channel = cid;
                r.direct = p.device.is_some();
                return Ok(r);
            }
            if let Some(bytes) = bytes {
                let kind = info.media_kind().ok_or_else(|| ApiError::invalid("an image of a kind this version cannot read"))?;
                if p.png && kind == trs_media::Kind::Svg {
                    let v = tokio::task::spawn_blocking(move || trs_media::render(kind, &bytes, 0)).await.map_err(|_| ApiError::internal("the drawing stopped"))?;
                    let v = v.map_err(|e| ApiError::invalid(e.to_string()))?;
                    (d.mime, d.base64, d.rendered) = (Some(v.mime.into()), Some(crate::b64::encode(&v.bytes)), true);
                } else {
                    (d.mime, d.base64) = (Some(kind.mime().into()), Some(crate::b64::encode(&bytes)));
                }
            }
            let mut r = reply(&d)?;
            r.note = Some(match d.base64 {
                Some(_) => format!("image {} ({}) of {}", clip(&d.attachment.name, 80), short(&d.attachment.hash), clip(&name, 80)),
                None => format!("image {} is {}: ask again in a moment", short(&d.attachment.hash), d.attachment.state.replace('_', " ")),
            });
            r.channel = cid;
            r.direct = p.device.is_some();
            Ok(r)
        }
        Request::ChatDownload(p) => {
            let (place, cid, name) = place_for(&h, &p.channel, p.device.as_deref(), "chat.download")?;
            let to = match p.out.as_deref() {
                None => None,
                Some(o) if !std::path::Path::new(o).is_absolute() => return Err(ApiError::params("`out` must be a full path")),
                Some(o) if std::path::Path::new(o).is_dir() => Some(trs_sync_net::files::DownloadTo::Folder(o.into())),
                Some(o) => Some(trs_sync_net::files::DownloadTo::File(o.into())),
            };
            let info = h.chat_file_download(place, &p.hash, to).map_err(chat_err)?;
            file_reply(place, cid, &name, &info, p.device.is_some())
        }
        Request::ChatCancelDownload(p) => {
            let (place, cid, name) = place_for(&h, &p.channel, p.device.as_deref(), "chat.cancel_download")?;
            let info = h.chat_file_cancel(place, &p.hash).map_err(chat_err)?;
            file_reply(place, cid, &name, &info, p.device.is_some())
        }
        Request::ChatStopSharing(p) => {
            let (place, cid, name) = place_for(&h, &p.channel, p.device.as_deref(), "chat.stop_sharing")?;
            let info = h.chat_file_stop_sharing(place, &p.hash).map_err(chat_err)?;
            file_reply(place, cid, &name, &info, p.device.is_some())
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
            let items = attachments_of(&[], &p.files, p.json)?;
            let m = if items.is_empty() {
                h.chat_send_direct(&dev, kind, &p.body, p.reply_to.as_deref(), Some(&agent)).await.map_err(chat_err)?
            } else {
                h.chat_send_attachments(trs_sync_net::room::Place::Direct(dev), None, &p.body, p.reply_to.as_deref(), Some(&agent), items).await.map_err(chat_err)?
            };
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
                // A reply to a request's announcement: the desk sees whether the user answered (off this task: it may
                // change settings).
                Ok(e) => {
                    if e.message.reply_to.as_deref().is_some_and(|r| ctx.desk.announced_by(&e.message.channel_id, r).is_some()) {
                        let (desk, h, m) = (ctx.desk.clone(), ctx.backend.sync(), e.message.clone());
                        tokio::task::spawn_blocking(move || {
                            let paired = h.map(|h| paired_devices(&h)).unwrap_or_default();
                            let _ = desk.on_message(&facts(&m), &paired);
                        });
                    }
                    if e.agents {
                        ctx.hub.publish_with(
                            EventDto {
                                kind: "chat.message".into(),
                                // A private message: the other device, so a stream can be filtered by conversation.
                                id: Some(e.message.peer_device.clone().unwrap_or_else(|| e.message.channel_id.clone())),
                                name: Some(if e.message.direct { e.message.author_name.clone() } else { e.message.channel_name.clone() }),
                                data: serde_json::to_value(message_dto(&e.message)).unwrap_or_default(),
                                ..Default::default()
                            },
                            false,
                        );
                    }
                }
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

// MARK: Requests for access (`agents.request`)

/// The user's other devices (*Your devices*, paired in the app): a post of the user's in the app there approves a
/// request here too. None while this device is not paired.
fn paired_devices(h: &SyncHandle) -> Vec<String> {
    let d = h.devices_status();
    if d.state != trs_sync_net::devices::DevicesState::Paired {
        return Vec::new();
    }
    d.devices.iter().filter(|x| !x.is_me).map(|x| x.id.to_string()).collect()
}

/// What the desk reads of a message.
fn facts(m: &trs_sync_net::chat::MessageInfo) -> crate::access::MessageFacts<'_> {
    crate::access::MessageFacts {
        channel: &m.channel_id,
        direct: m.direct,
        kind: &m.kind,
        reply_to: m.reply_to.as_deref(),
        body: &m.body,
        trust: &m.trust,
        agent: m.agent.as_deref(),
        me: m.me,
        author_device: &m.author_device,
        author_name: &m.author_name,
    }
}

/// Where a request is announced: a channel open to agents (and its topic).
pub(super) struct AnnouncePlace {
    id: FolderId,
    topic: Option<String>,
}

/// The channel `channel` names for an announcement, among those open to agents.
pub(super) fn announce_place<B: Backend>(ctx: &Ctx<B>, channel: &str, topic: Option<&str>) -> Result<AnnouncePlace, ApiError> {
    let h = ctx.backend.sync().ok_or_else(s::not_running)?;
    let (_, id) = pick(&h, channel)?;
    Ok(AnnouncePlace { id, topic: topic.map(str::to_owned) })
}

/// The announcement's text: who asks what on which device, why (the agent's words, quoted), and how to answer.
pub fn announcement_text(e: &crate::access::Entry, device: &str) -> String {
    let device = if device.trim().is_empty() { concat!("this ", trs_platform::computer!()) } else { device.trim() };
    let mut t = format!("**{} asks for {} on {device}.**\n", e.who(), e.want.text());
    if let Some(w) = &e.why {
        t.push_str(&format!("\n> {}\n", w.replace('\n', " ")));
    }
    t.push_str(&format!(
        "\nReply to this message with **yes** to allow it, or **no** to refuse. Only your own reply in Seedwell counts; an agent's never does. You can also answer in Seedwell on {device} (its notification, or Settings › Agents). Unanswered, it expires in a day."
    ));
    t
}

/// Posts request `e`'s announcement (under [`crate::access::ANNOUNCER`]): the channel's id and the message's.
pub(super) async fn announce<B: Backend>(ctx: &Ctx<B>, place: &AnnouncePlace, e: &crate::access::Entry, reply_to: Option<&str>) -> Result<(String, String), ApiError> {
    let h = ctx.backend.sync().ok_or_else(s::not_running)?;
    let body = announcement_text(e, &h.settings().device_name);
    let m = h.chat_send(&place.id, place.topic.as_deref(), MessageKind::Text, &body, reply_to, Some(crate::access::ANNOUNCER)).await.map_err(chat_err)?;
    Ok((place.id.to_string(), m.id))
}

/// Folder sync's part of answering requests: channels and private messages.
pub(super) struct SyncApplier<B> {
    backend: Arc<B>,
}

impl<B> SyncApplier<B> {
    pub(super) fn new(backend: Arc<B>) -> Self {
        Self { backend }
    }
}

impl<B: Backend> SyncApplier<B> {
    /// The one channel `name` names among all on this device (closed ones too); `None` when none or several.
    fn channel(&self, name: &str) -> Option<(SyncHandle, ChannelStatus)> {
        let h = self.backend.sync()?;
        let all = h.chat_channels();
        let c = resolve_channel(&all, name).ok()?.clone();
        Some((h, c))
    }
}

impl<B: Backend> crate::access::Applier for SyncApplier<B> {
    fn open_channel(&self, name: &str) -> Result<String, String> {
        let (h, c) = self.channel(name).ok_or_else(|| format!("there is no channel “{}” on this {}", clip(name, 80), trs_platform::computer!()))?;
        if c.agents {
            return Ok(format!("the channel “{}” is open to agents already", c.name));
        }
        if !c.role.is_some_and(Role::admits) {
            return Err(format!("only the owners and admins of “{}” can open it to agents; answer on one of their devices", c.name));
        }
        let id: FolderId = c.id.parse().map_err(|_| "the channel's id is malformed".to_owned())?;
        h.chat_set_agents(&id, true).map_err(|e| e.to_string())?;
        Ok(format!("the channel “{}” is open to agents now (Agents: Yes, on every device)", c.name))
    }

    fn dm_agents(&self, post: bool) -> Result<String, String> {
        let h = self.backend.sync().ok_or("private messages are not running")?;
        let want = if post { AgentAccess::ReadPost } else { AgentAccess::Read };
        if h.chat_direct_agents() >= want {
            return Ok("agents have that access to private messages already".into());
        }
        h.chat_set_direct_agents(want).map_err(|e| e.to_string())?;
        Ok(if post { "agents may read and post private messages now" } else { "agents may read private messages now" }.into())
    }

    fn channel_askable(&self, name: &str) -> bool {
        self.channel(name).is_some_and(|(_, c)| !c.agents && c.role.is_some_and(Role::admits))
    }

    fn channel_open(&self, name: &str) -> bool {
        self.channel(name).is_some_and(|(_, c)| c.agents)
    }

    fn dm_has(&self, post: bool) -> Option<bool> {
        let h = self.backend.sync()?;
        Some(h.chat_direct_agents() >= if post { AgentAccess::ReadPost } else { AgentAccess::Read })
    }
}
