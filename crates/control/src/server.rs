//! The control server: accepts same-user connections on the control socket and serves the protocol
//! on top of a [`Backend`].
//!
//! Per connection: `hello` first, then one request at a time (responses are in request order),
//! pushed events interleaved after `events_subscribe`. Every call is permission-checked against the
//! *current* level, rate limited, and, when it mutates, written to the activity log.

use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader, WriteHalf};
use tokio::sync::{Semaphore, broadcast, watch};
use tokio::task::JoinSet;

use crate::access::AccessDesk;
use crate::activity::{ActivityLog, describe};
use crate::awake::Awake;
use crate::backend::{AddArgs, AddSource, Backend};
use crate::confirm::{Confirmations, Plan, SyncPlan, TokenError};
use crate::limits::{LineRead, RateLimiter, read_line};
use crate::ops::{self, Picked, SourceKind};
use crate::perm::{Permission, PermissionSource};
use crate::proto::*;
use crate::redact::scrub_urls;
use crate::transport::{self, Listener, Owner};
use crate::state::StateAccess;
use crate::types::*;

/// Capacity of the recent-events buffer served by `events`.
const EVENT_RING: usize = 1024;

/// Tunables. The defaults are the product's.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Simultaneous clients (SPEC 9A / brief: 8). The ninth is told why and disconnected.
    pub max_connections: usize,
    /// Largest request line after `hello`. Must hold a base64 `.torrent` (16 MiB torrent = 22 MiB).
    pub max_line_bytes: usize,
    /// Largest line before `hello` completes (a hello is tiny).
    pub prehello_line_bytes: usize,
    /// Calls per second per connection (`agents.rate_limit_per_sec`, default 20).
    pub rate_per_sec: u32,
    /// Bucket size.
    pub burst: u32,
    /// How long a removal token lives.
    pub confirm_ttl: Duration,
    /// A client that does not read its responses for this long is disconnected.
    pub write_timeout: Duration,
    /// `seedwell <version>`, as `hello` and `status` report it.
    pub server_name: String,
    /// Consecutive unparseable lines before the connection is dropped.
    pub max_strikes: u32,
    /// The host holds a power assertion while an agent's `awake.set` lease is live (the app does; a
    /// headless daemon cannot, and says so).
    pub keeps_awake: bool,
    /// The app's updater (`update.*`); a daemon leaves it empty.
    pub builds: crate::builds::BuildsSlot,
    /// The app's torrent proxy (`proxy.*`); a daemon leaves it empty.
    pub proxy: crate::proxy::ProxySlot,
    /// Looks for other Seedwells on this computer for `status` (`others`), given this one's state folder: the user's
    /// app sets it (`crate::others::line_now`); a daemon and an isolated (test) run leave it out, so a test never
    /// reports the user's own app.
    pub others: Option<fn(&Path) -> Option<String>>,
    /// The file the activity log is kept in across restarts (the app's and the daemon's); none: in memory only.
    pub activity_file: Option<PathBuf>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            max_connections: 8,
            max_line_bytes: 32 << 20,
            prehello_line_bytes: 4096,
            rate_per_sec: 20,
            burst: 40,
            confirm_ttl: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
            server_name: format!("seedwell {}", env!("CARGO_PKG_VERSION")),
            max_strikes: 8,
            keeps_awake: false,
            builds: crate::builds::BuildsSlot::new(),
            proxy: crate::proxy::ProxySlot::new(),
            others: None,
            activity_file: None,
        }
    }
}

impl ServerOptions {
    /// Take the per-client rate (`agents.rate_limit_per_sec`) from the settings.
    pub fn with_config(mut self, cfg: &seedwell_state::Config) -> Self {
        self.rate_per_sec = cfg.agents.rate_limit_per_sec.max(1);
        self.burst = self.rate_per_sec.saturating_mul(2).max(10);
        self
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn now_secs() -> u64 {
    now_ms() / 1000
}

/// Recent events plus the live fan-out.
struct EventHub {
    inner: Mutex<(u64, VecDeque<Arc<EventDto>>)>,
    tx: broadcast::Sender<Arc<EventDto>>,
}

impl EventHub {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(512);
        Self { inner: Mutex::new((0, VecDeque::new())), tx }
    }

    fn publish(&self, e: EventDto) {
        self.publish_with(e, true);
    }

    /// `scrub`: mask URL secrets and cap the strings in `data` (engine events). A channel message is not: it
    /// arrives as `chat.read` gives it.
    fn publish_with(&self, mut e: EventDto, scrub: bool) {
        // Hold the lock across the send so sequence order is delivery order.
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.0 += 1;
        e.seq = g.0;
        e.at = now_ms();
        if let Some(m) = &e.name {
            e.name = Some(clip(&ops::clean_name(m), 300));
        }
        if scrub {
            e.data = scrub_value(e.data);
        }
        let e = Arc::new(e);
        if g.1.len() >= EVENT_RING {
            g.1.pop_front();
        }
        g.1.push_back(e.clone());
        let _ = self.tx.send(e);
    }

    fn cursor(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).0
    }

    fn since(&self, since: u64, limit: usize, kinds: Option<&HashSet<String>>) -> EventsDto {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let oldest = g.1.front().map_or(g.0 + 1, |e| e.seq);
        let events: Vec<EventDto> = g
            .1
            .iter()
            .filter(|e| e.seq > since && kinds.is_none_or(|k| k.contains(&e.kind)))
            .take(limit)
            .map(|e| (**e).clone())
            .collect();
        // With nothing new the cursor is the newest seq, so a poller does not rescan.
        let cursor = events.last().map_or(g.0.max(since), |e| e.seq);
        EventsDto { events, cursor, oldest_available: oldest }
    }
}

fn scrub_value(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(clip(&scrub_urls(&s), 500)),
        Value::Array(a) => Value::Array(a.into_iter().take(64).map(scrub_value).collect()),
        Value::Object(o) => Value::Object(o.into_iter().take(64).map(|(k, v)| (k, scrub_value(v))).collect()),
        other => other,
    }
}

struct Ctx<B> {
    backend: Arc<B>,
    state: StateAccess,
    perm: PermissionSource,
    opts: ServerOptions,
    activity: ActivityLog,
    awake: Awake,
    confirms: Mutex<Confirmations>,
    /// Agents' requests for access (`agents.request`); the app answers them.
    desk: AccessDesk,
    /// `sync.remove` plans (their tokens never remove a torrent, and the other way round).
    #[cfg_attr(not(feature = "sync"), allow(dead_code))]
    sync_confirms: Mutex<Confirmations<SyncPlan>>,
    hub: Arc<EventHub>,
    connections: AtomicUsize,
    calls: AtomicU64,
    stop: watch::Receiver<bool>,
    /// What open connections hold alone ([`EventsSubscribeParams::hold`]): by name, the connection and who holds it.
    holds: Mutex<std::collections::HashMap<String, Holder>>,
    /// Numbers the connections (for `holds`).
    conn_ids: AtomicU64,
    /// When this server started answering (Unix ms), told in `hello`: a restart's new run has a later one.
    started_ms: u64,
    /// Receivers of the sync service's channel messages for the chat pump, each made before the request that made it
    /// answers ([`chat_methods::tap`]), so an agent that subscribed misses no message posted after.
    #[cfg(feature = "sync")]
    chat_taps: tokio::sync::mpsc::UnboundedSender<broadcast::Receiver<seedwell_sync_net::chat::ChatEvent>>,
}

/// A running server. Dropping it stops accepting, closes connections and removes the socket.
pub struct ServerHandle {
    path: PathBuf,
    owner: Owner,
    warnings: Vec<String>,
    activity: ActivityLog,
    awake: Awake,
    desk: AccessDesk,
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
    connections: Arc<dyn Fn() -> usize + Send + Sync>,
    calls: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl ServerHandle {
    /// The socket path clients connect to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The agent activity log (the app shows it in Settings -> Agents).
    pub fn activity(&self) -> &ActivityLog {
        &self.activity
    }

    /// Agents' leases on keeping the Mac awake (the app holds a power assertion while one is live).
    pub fn awake(&self) -> &Awake {
        &self.awake
    }

    /// Agents' requests for access: what the app shows and answers (no control method answers them).
    pub fn access(&self) -> &AccessDesk {
        &self.desk
    }

    /// Notes from startup (for example a loosely protected parent directory).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Clients connected right now.
    pub fn connections(&self) -> usize {
        (self.connections)()
    }

    /// Requests handled so far.
    pub fn calls(&self) -> u64 {
        (self.calls)()
    }

    /// Stop and wait for connections to close (at most two seconds), then remove the socket.
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(t) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), t).await;
        }
        transport::remove_if_ours(&self.path, &self.owner);
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(t) = &self.task {
            t.abort();
        }
        transport::remove_if_ours(&self.path, &self.owner);
    }
}

/// Start serving `backend` on `path`. The permission level is read from `perm` on every call;
/// at `off` clients are refused.
pub async fn serve_backend<B: Backend>(
    backend: Arc<B>,
    state: StateAccess,
    path: impl AsRef<Path>,
    perm: PermissionSource,
    opts: ServerOptions,
) -> io::Result<ServerHandle> {
    let path = path.as_ref().to_path_buf();
    let bound = transport::bind(&path).await?;
    let (stop_tx, stop_rx) = watch::channel(false);
    let activity = opts.activity_file.clone().map_or_else(ActivityLog::new, ActivityLog::persisted);
    let awake = Awake::new();
    #[cfg(feature = "sync")]
    let (chat_taps, chat_taps_rx) = tokio::sync::mpsc::unbounded_channel();
    let hub = Arc::new(EventHub::new());
    let desk = {
        let hub = hub.clone();
        AccessDesk::new(activity.clone(), state.clone(), opts.builds.clone(), Arc::new(move |e| hub.publish(e)))
    };
    #[cfg(feature = "sync")]
    desk.set_applier(Arc::new(chat_methods::SyncApplier::new(backend.clone())));
    let ctx = Arc::new(Ctx {
        backend,
        state,
        perm,
        desk: desk.clone(),
        activity: activity.clone(),
        awake: awake.clone(),
        confirms: Mutex::new(Confirmations::new(opts.confirm_ttl)),
        sync_confirms: Mutex::new(Confirmations::new(opts.confirm_ttl)),
        hub,
        connections: AtomicUsize::new(0),
        calls: AtomicU64::new(0),
        stop: stop_rx.clone(),
        holds: Mutex::new(std::collections::HashMap::new()),
        conn_ids: AtomicU64::new(0),
        started_ms: now_ms(),
        #[cfg(feature = "sync")]
        chat_taps,
        opts,
    });
    // One pump from the engine's event stream into the hub.
    let mut engine_events = ctx.backend.subscribe();
    let hub_ctx = ctx.clone();
    let mut pump_stop = stop_rx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = pump_stop.changed() => break,
                r = engine_events.recv() => match r {
                    Ok(e) => hub_ctx.hub.publish(e),
                    Err(broadcast::error::RecvError::Lagged(n)) => hub_ctx.hub.publish(EventDto {
                        kind: "lagged".into(),
                        data: json!({ "missed": n }),
                        ..Default::default()
                    }),
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
    // Channel messages, for agents whose channels allow it.
    #[cfg(feature = "sync")]
    tokio::spawn(chat_methods::pump(ctx.clone(), chat_taps_rx, stop_rx.clone()));
    let c1 = ctx.clone();
    let c2 = ctx.clone();
    let task = tokio::spawn(accept_loop(ctx, bound.listener));
    Ok(ServerHandle {
        path,
        owner: bound.owner,
        warnings: bound.warnings,
        activity,
        awake,
        desk,
        stop: stop_tx,
        task: Some(task),
        connections: Arc::new(move || c1.connections.load(Ordering::Relaxed)),
        calls: Arc::new(move || c2.calls.load(Ordering::Relaxed)),
    })
}

async fn accept_loop<B: Backend>(ctx: Arc<Ctx<B>>, mut listener: Listener) {
    let slots = Arc::new(Semaphore::new(ctx.opts.max_connections));
    let mut conns = JoinSet::new();
    let mut stop = ctx.stop.clone();
    loop {
        let accepted = tokio::select! {
            _ = stop.changed() => break,
            r = listener.accept() => r,
            // Reap finished connection tasks so the set does not grow.
            Some(_) = conns.join_next(), if !conns.is_empty() => continue,
        };
        // Only our own user gets this far (the transport checks).
        let stream = match accepted {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        match slots.clone().try_acquire_owned() {
            Ok(permit) => {
                let ctx = ctx.clone();
                conns.spawn(async move {
                    ctx.connections.fetch_add(1, Ordering::Relaxed);
                    serve_conn(&ctx, stream).await;
                    ctx.connections.fetch_sub(1, Ordering::Relaxed);
                    drop(permit);
                });
            }
            Err(_) => {
                let to = ctx.opts.write_timeout.min(Duration::from_secs(2));
                conns.spawn(async move {
                    let line = ServerLine::err(
                        None,
                        ApiError::new(ErrorCode::TooManyConnections, "too many clients are connected; close one and retry"),
                    );
                    let mut s = stream;
                    let _ = tokio::time::timeout(to, s.write_all(&line.to_line())).await;
                });
            }
        }
    }
    // Let connection tasks see the stop signal and finish.
    let _ = tokio::time::timeout(Duration::from_secs(2), async { while conns.join_next().await.is_some() {} }).await;
    conns.abort_all();
}

struct Sub {
    rx: broadcast::Receiver<Arc<EventDto>>,
    kinds: Option<HashSet<String>>,
}

#[derive(Default)]
struct Conn {
    id: u64,
    client: Option<String>,
    sub: Option<Sub>,
    strikes: u32,
    close: bool,
}

/// A connection that holds a name ([`EventsSubscribeParams::hold`]).
struct Holder {
    conn: u64,
    client: String,
    since_s: u64,
}

/// Takes `name` for connection `conn` (again: fine); `Err` names the other open connection that holds it.
fn take_hold(holds: &Mutex<std::collections::HashMap<String, Holder>>, name: &str, conn: u64, client: &str, now_s: u64) -> Result<(), ApiError> {
    let mut h = holds.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(other) = h.get(name).filter(|o| o.conn != conn) {
        return Err(ApiError::new(ErrorCode::Duplicate, format!("another connection ({}) holds this since {}", other.client, other.since_s))
            .with_data(json!({ "client": other.client, "since": other.since_s })));
    }
    h.insert(name.to_owned(), Holder { conn, client: client.to_owned(), since_s: now_s });
    Ok(())
}

/// Lets go of everything connection `conn` holds (it closed).
fn release_holds(holds: &Mutex<std::collections::HashMap<String, Holder>>, conn: u64) {
    holds.lock().unwrap_or_else(|p| p.into_inner()).retain(|_, h| h.conn != conn);
}

async fn next_event(sub: &mut Option<Sub>) -> Result<Arc<EventDto>, broadcast::error::RecvError> {
    match sub {
        Some(s) => loop {
            match s.rx.recv().await {
                Ok(e) if s.kinds.as_ref().is_none_or(|k| k.contains(&e.kind)) => return Ok(e),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        },
        None => std::future::pending().await,
    }
}

async fn send(wr: &mut WriteHalf<transport::Conn>, bytes: &[u8], timeout: Duration) -> bool {
    matches!(tokio::time::timeout(timeout, wr.write_all(bytes)).await, Ok(Ok(())))
}

async fn serve_conn<B: Backend>(ctx: &Arc<Ctx<B>>, stream: transport::Conn) {
    let (rd, mut wr) = tokio::io::split(stream);
    let mut rd = BufReader::with_capacity(32 * 1024, rd);
    let wt = ctx.opts.write_timeout;
    if ctx.perm.get() == Permission::Off {
        let e = ApiError::new(
            ErrorCode::AgentAccessOff,
            "agent access is switched off; enable it in Seedwell Settings -> Agents",
        );
        let _ = send(&mut wr, &ServerLine::err(None, e).to_line(), wt).await;
        return;
    }
    let mut limiter = RateLimiter::new(ctx.opts.rate_per_sec, ctx.opts.burst, Instant::now());
    let mut conn = Conn { id: ctx.conn_ids.fetch_add(1, Ordering::Relaxed) + 1, ..Conn::default() };
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut stop = ctx.stop.clone();
    loop {
        let cap = if conn.client.is_some() { ctx.opts.max_line_bytes } else { ctx.opts.prehello_line_bytes };
        tokio::select! {
            _ = stop.changed() => break,
            r = read_line(&mut rd, &mut buf, cap) => match r {
                Ok(LineRead::Line) => {
                    if buf.iter().all(u8::is_ascii_whitespace) {
                        buf.clear();
                        continue;
                    }
                    let out = handle_line(ctx, &mut conn, &mut limiter, &buf).await;
                    buf.clear();
                    if !send(&mut wr, &out, wt).await || conn.close {
                        break;
                    }
                }
                Ok(LineRead::TooLong) => {
                    let e = ApiError::new(ErrorCode::RequestTooLarge, format!("request line is longer than {cap} bytes"));
                    let _ = send(&mut wr, &ServerLine::err(None, e).to_line(), wt).await;
                    break;
                }
                Ok(LineRead::Eof) | Err(_) => break,
            },
            ev = next_event(&mut conn.sub) => {
                let line = match ev {
                    Ok(e) if !event_visible(ctx, &e) => continue,
                    Ok(e) => ServerLine::event(&e),
                    Err(broadcast::error::RecvError::Lagged(n)) => ServerLine::event(&EventDto {
                        kind: "lagged".into(),
                        data: json!({ "missed": n }),
                        seq: ctx.hub.cursor(),
                        ..Default::default()
                    }),
                    Err(broadcast::error::RecvError::Closed) => { conn.sub = None; continue; }
                };
                if !send(&mut wr, &line.to_line(), wt).await {
                    break;
                }
            }
        }
    }
    release_holds(&ctx.holds, conn.id);
}

/// A successful handler result.
struct Reply {
    value: Value,
    /// Extra text for the activity log (names of what was removed, ...).
    note: Option<String>,
    /// The channel the call was about (its id), so an agent's `activity` leaves the entry out while the channel is
    /// closed to agents ([`ActivityDto::channel`]).
    channel: Option<String>,
    /// The call was about private messages ([`ActivityDto::direct`]).
    direct: bool,
}

fn reply<T: Serialize>(v: &T) -> Result<Reply, ApiError> {
    serde_json::to_value(v)
        .map(|value| Reply { value, note: None, channel: None, direct: false })
        .map_err(|e| ApiError::internal(format!("cannot encode result: {e}")))
}

/// An event an agent may be given now. A `chat.message` was published only while its channel (or, for a private
/// message, the Mac's switch) let agents read, but the user may have switched that off since: it is checked again
/// whenever the event is handed out, live or replayed by `events`.
fn event_visible<B: Backend>(ctx: &Ctx<B>, e: &EventDto) -> bool {
    e.kind != "chat.message" || chat_event_visible(ctx, e)
}

#[cfg(feature = "sync")]
fn chat_event_visible<B: Backend>(ctx: &Ctx<B>, e: &EventDto) -> bool {
    ctx.backend.sync().is_some_and(|h| chat_methods::message_visible(&h, &e.data))
}

#[cfg(not(feature = "sync"))]
fn chat_event_visible<B: Backend>(_ctx: &Ctx<B>, _e: &EventDto) -> bool {
    false
}

/// An activity entry an agent may be given now: none about a channel, or private messages, closed to agents.
#[cfg(feature = "sync")]
fn activity_visible<B: Backend>(ctx: &Ctx<B>, e: &ActivityDto) -> bool {
    if e.channel.is_none() && !e.direct {
        return true;
    }
    ctx.backend.sync().is_some_and(|h| {
        let channel_ok = e.channel.as_deref().is_none_or(|id| chat_methods::channel_visible(&h, id));
        channel_ok && (!e.direct || chat_methods::directs_visible(&h))
    })
}

#[cfg(not(feature = "sync"))]
fn activity_visible<B: Backend>(_ctx: &Ctx<B>, e: &ActivityDto) -> bool {
    e.channel.is_none() && !e.direct
}

async fn handle_line<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &mut Conn, limiter: &mut RateLimiter, line: &[u8]) -> Vec<u8> {
    ctx.calls.fetch_add(1, Ordering::Relaxed);
    if let Err(wait) = limiter.acquire(Instant::now()) {
        let e = ApiError::new(ErrorCode::RateLimited, "too many requests; slow down")
            .with_data(json!({ "retry_after_ms": wait.as_millis() as u64 + 1 }));
        return ServerLine::err(None, e).to_line();
    }
    let envelope = match decode_envelope(line) {
        Ok(d) => {
            conn.strikes = 0;
            d
        }
        Err(f) => {
            conn.strikes += 1;
            if conn.strikes >= ctx.opts.max_strikes {
                conn.close = true;
            }
            return ServerLine::err(f.id, f.error).to_line();
        }
    };
    let id = envelope.id.clone();
    let method = envelope.method;
    let client = conn.client.clone();

    // Permission first, params second: a caller without permission learns nothing else.
    let mut request = None;
    let mut denied = false;
    let result = match authorize(ctx, conn, method) {
        Err(e) => {
            denied = true;
            Err(e)
        }
        Ok(()) => match envelope.decode() {
            Err(f) => {
                conn.strikes += 1;
                Err(f.error)
            }
            Ok(d) => {
                request = Some(d.request.clone());
                dispatch(ctx, conn, d.request).await
            }
        },
    };
    if method.mutating() {
        let who = client.or_else(|| conn.client.clone()).unwrap_or_else(|| "unknown".into());
        let (outcome, note, channel, direct) = match &result {
            Ok(r) => ("ok".to_owned(), r.note.clone(), r.channel.clone(), r.direct),
            Err(_) if denied => ("denied".to_owned(), None, None, false),
            Err(e) => (format!("error: {}", e.code.as_str()), None, None, false),
        };
        let mut summary = request.as_ref().map_or_else(|| format!("{} (refused)", method.name()), describe);
        if let Some(n) = note {
            summary.push_str(" -> ");
            summary.push_str(&n);
        }
        ctx.activity.push(ActivityDto {
            at: now_ms(),
            client: who,
            method: method.name().to_owned(),
            // Folder keys never reach the log, whatever field they were pasted into.
            summary: clip(&crate::redact::scrub_keys(&summary), 600),
            outcome,
            channel,
            direct,
        });
    }
    match result {
        Ok(r) => ServerLine::ok(Some(id), r.value).to_line(),
        Err(e) => {
            if e.code == ErrorCode::AgentAccessOff {
                conn.close = true;
            }
            ServerLine::err(Some(id), e).to_line()
        }
    }
}

/// Handshake and permission gate in one place so no method can skip either.
fn authorize<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, method: Method) -> Result<(), ApiError> {
    if conn.client.is_none() && method != Method::Hello {
        return Err(ApiError::new(ErrorCode::HelloRequired, "the first message must be `hello` with your client name"));
    }
    let current = ctx.perm.get();
    if current == Permission::Off {
        return Err(ApiError::new(ErrorCode::AgentAccessOff, "agent access was switched off in Seedwell Settings -> Agents"));
    }
    if !current.allows(method.required()) {
        return Err(denied(method.name(), method.required(), current));
    }
    Ok(())
}

fn denied(method: &str, required: Permission, current: Permission) -> ApiError {
    let ask = format!("seedwell agents request {}", crate::access::level_name(required));
    ApiError::new(
        ErrorCode::PermissionDenied,
        format!(
            "`{method}` needs agent permission `{}` but the current level is `{}`; ask the user for it with `{ask} --why \"<what for>\"` (one click or a chat reply from the user grants it; `seedwell agents requests` shows the answer)",
            required.as_str(),
            current.as_str()
        ),
    )
    .with_data(json!({ "required": required.as_str(), "current": current.as_str(), "ask": ask }))
}

async fn rows<B: Backend>(ctx: &Ctx<B>) -> Result<(Vec<TransferDto>, TotalsDto), ApiError> {
    let snap = ctx.backend.snapshot().await?;
    Ok((snap.transfers, snap.totals))
}

fn names(p: &[Picked]) -> Vec<IdName> {
    p.iter().map(|p| IdName { id: id_hex(&p.id), name: p.name.clone() }).collect()
}

fn ids_of(p: &[Picked]) -> Vec<TransferId> {
    p.iter().map(|p| p.id).collect()
}

fn action(what: &str, picked: &[Picked]) -> Result<Reply, ApiError> {
    let mut r = reply(&ActionDto { action: what.to_owned(), count: picked.len() as u32, transfers: names(picked) })?;
    r.note = Some(format!("{what} {}", picked.iter().take(5).map(|p| clip(&p.name, 60)).collect::<Vec<_>>().join(", ")));
    Ok(r)
}

/// A custom save folder lets the caller decide where downloaded bytes land, so it needs `full`;
/// at `control` downloads go where the user's settings and rules put them. Locations where a
/// file could become code or configuration (`~/Library`, dot-folders, system paths) are refused
/// at every level.
fn expand_save_path(p: &Option<String>, level: Permission) -> Result<Option<PathBuf>, ApiError> {
    let Some(raw) = p.as_deref() else { return Ok(None) };
    if !level.allows(Permission::Full) {
        return Err(denied("add.save_path", Permission::Full, level));
    }
    let path = ops::clean_path(raw, "save_path")?;
    if ops::is_sensitive_dir(&path) {
        return Err(ApiError::invalid("`save_path` points at a system, Library or hidden folder; choose a normal folder"));
    }
    Ok(Some(path))
}

/// The app's torrent proxy, for `proxy.*`.
fn proxy<B: Backend>(ctx: &Ctx<B>) -> Result<std::sync::Arc<dyn crate::proxy::ProxyControl>, ApiError> {
    ctx.opts.proxy.get().ok_or_else(|| ApiError::new(ErrorCode::Unsupported, "this Seedwell has no torrent proxy (the app has; a daemon has not)"))
}

/// What a `proxy.*` call did: the proxy as it is now, with the sentence.
async fn proxy_reply<B: Backend>(ctx: &Ctx<B>, call: impl FnOnce(&dyn crate::proxy::ProxyControl) -> Result<String, String> + Send + 'static) -> Result<Reply, ApiError> {
    let p = proxy(ctx)?;
    let out = blocking(move || -> Result<ProxyDto, ApiError> {
        let note = call(&*p).map_err(ApiError::invalid)?;
        Ok(ProxyDto { note, ..p.status() })
    })
    .await??;
    reply(&out)
}

/// The app's updater, for `update.*` and `restart`: in an app still starting, once it registered; a daemon says
/// `daemon_says`.
async fn updater<B: Backend>(ctx: &Ctx<B>, daemon_says: &str) -> Result<std::sync::Arc<dyn crate::builds::Builds>, ApiError> {
    ctx.opts.builds.wait(crate::builds::STARTING).await.map_err(|why| match why {
        crate::builds::NoBuilds::Daemon => ApiError::new(ErrorCode::Unsupported, daemon_says),
        crate::builds::NoBuilds::Starting => ApiError::new(ErrorCode::Timeout, "Seedwell is still starting: try again in a moment"),
    })
}

const NO_BUILDS: &str = "this Seedwell installs no builds (the app does; a daemon does not)";

/// A path given to `update.*`: absolute, no `..`.
fn absolute(p: &str) -> Result<PathBuf, ApiError> {
    let path = PathBuf::from(p.trim());
    if !path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(ApiError::params("give an absolute path without `..`"));
    }
    Ok(path)
}

async fn awake_status<B: Backend>(ctx: &Arc<Ctx<B>>) -> Result<Reply, ApiError> {
    let st = ctx.state.clone();
    let setting = blocking(move || st.config().general.keep_awake_for_agents).await?;
    let leases = ctx.awake.leases();
    let host_keeps = ctx.opts.keeps_awake;
    reply(&AwakeDto { kept: setting && host_keeps && !leases.is_empty(), leases, setting, host_keeps })
}

async fn dispatch<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &mut Conn, req: Request) -> Result<Reply, ApiError> {
    let b = &ctx.backend;
    match req {
        Request::Hello(p) => {
            let name = p.client.trim();
            if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
                return Err(ApiError::params("`client` must be 1-64 characters without control characters"));
            }
            if p.protocol == Some(0) {
                return Err(ApiError::new(ErrorCode::UnsupportedVersion, "protocol 0 does not exist"));
            }
            conn.client = Some(name.to_owned());
            let level = ctx.perm.get();
            reply(&HelloDto {
                protocol: PROTOCOL_VERSION,
                server: ctx.opts.server_name.clone(),
                permission: level.as_str().to_owned(),
                methods: Method::ALL.iter().filter(|m| level.allows(m.required())).map(|m| m.name().to_owned()).collect(),
                rate_limit_per_sec: ctx.opts.rate_per_sec,
                pid: std::process::id(),
                started_ms: ctx.started_ms,
            })
        }
        Request::Ping(_) => reply(&json!({ "pong": true, "time_ms": now_ms() })),
        Request::Status(_) => {
            let (rows, totals) = rows(ctx).await?;
            let limits = b.limits().await?;
            let counts = ops::counts_by_state(&rows);
            let summary = format!(
                "down {}, up {}; {}",
                crate::human::rate(totals.down_rate),
                crate::human::rate(totals.up_rate),
                ops::state_summary(&counts)
            );
            let reachability = b.reachability().await;
            let others = match ctx.opts.others {
                Some(look) => {
                    let dir = ctx.state.dir().to_path_buf();
                    blocking(move || look(&dir)).await?
                }
                None => None,
            };
            reply(&StatusDto {
                reachability,
                others,
                server: ctx.opts.server_name.clone(),
                protocol: PROTOCOL_VERSION,
                permission: ctx.perm.get().as_str().to_owned(),
                total: rows.len() as u32,
                totals,
                counts,
                limits,
                summary,
            })
        }
        Request::List(p) => {
            let (rows, _) = rows(ctx).await?;
            let mut out = ops::list(&rows, &p);
            out.transfers.iter_mut().for_each(ops::sanitize_transfer);
            reply(&out)
        }
        Request::Info(p) => {
            let (rows, _) = rows(ctx).await?;
            let picked = ops::resolve_ids(&rows, std::slice::from_ref(&p.id))?;
            let mut d = b.detail(picked[0].id).await?;
            ops::sanitize_transfer(&mut d.transfer);
            if p.files == Some(false) {
                d.files.clear();
            }
            let max_files = p.max_files.unwrap_or(2000).min(20_000) as usize;
            if d.files.len() > max_files {
                d.files_truncated = (d.files.len() - max_files) as u32;
                d.files.truncate(max_files);
            }
            if p.trackers == Some(false) {
                d.trackers.clear();
            }
            for t in &mut d.trackers {
                t.url = seedwell_state::url::redact_url(&t.url);
                t.status = clip(&scrub_urls(&t.status), 300);
            }
            if p.peers == Some(false) {
                d.peers.clear();
            }
            let max_peers = p.max_peers.unwrap_or(100).min(2000) as usize;
            if d.peers.len() > max_peers {
                d.peers_truncated = (d.peers.len() - max_peers) as u32;
                d.peers.truncate(max_peers);
            }
            reply(&d)
        }
        Request::Trackers(_) => trackers(ctx).await,
        Request::TrackerStats(p) => tracker_stats(ctx, p).await,
        Request::Rules(_) => {
            let st = ctx.state.clone();
            let rules = blocking(move || st.rules()).await?;
            reply(&rules)
        }
        Request::SettingsGet(p) => {
            let st = ctx.state.clone();
            if let Some(file) = p.file {
                if p.keys.is_some() {
                    return Err(ApiError::params("give keys or a file, not both"));
                }
                let file = crate::settings::check_file(&file)?;
                let doc = blocking(move || crate::settings::doc_get(&st.rules(), &st.profiles(), file)).await??;
                let mut settings = serde_json::Map::new();
                settings.insert(file.to_owned(), doc);
                return reply(&SettingsDto { settings, changed: vec![], note: String::new() });
            }
            // The proxy's key is in the secret store, not the file: whether one is installed (never the key), with every
            // setting, by its name, or inside `network` and `network.proxy`.
            const KEY: &str = "network.proxy.key";
            let keys = p.keys;
            let rest: Option<Vec<String>> = keys.as_ref().map(|ks| ks.iter().filter(|k| *k != KEY).cloned().collect());
            let cfg = blocking(move || st.config()).await?;
            let mut map = crate::settings::get(&cfg, rest.as_deref())?;
            let wanted: Vec<String> = match &keys {
                None => vec![KEY.to_owned()],
                Some(ks) => ks.iter().filter(|k| crate::settings::under(KEY, k)).cloned().collect(),
            };
            if !wanted.is_empty() {
                let Some(px) = ctx.opts.proxy.get() else {
                    // Asked for by name: no proxy here (a daemon) says so; in a listing or a table it is left out.
                    if keys.as_ref().is_some_and(|ks| ks.iter().any(|k| k == KEY)) {
                        return Err(proxy(ctx).err().unwrap_or_else(|| ApiError::internal("no proxy")));
                    }
                    return reply(&SettingsDto { settings: map, changed: vec![], note: String::new() });
                };
                let has = blocking(move || px.status().has_key).await?;
                let v = Value::String(if has { "installed" } else { "none" }.into());
                for k in wanted {
                    match KEY.strip_prefix(k.as_str()).and_then(|r| r.strip_prefix('.')) {
                        // Inside a table that was asked for.
                        Some(inner) => {
                            let mut at = map.get_mut(&k);
                            let parts: Vec<&str> = inner.split('.').collect();
                            for part in &parts[..parts.len() - 1] {
                                at = at.and_then(|t| t.get_mut(*part));
                            }
                            if let (Some(Value::Object(t)), Some(last)) = (at, parts.last()) {
                                t.insert((*last).to_owned(), v.clone());
                            }
                        }
                        None => {
                            map.insert(k, v.clone());
                        }
                    }
                }
            }
            reply(&SettingsDto { settings: map, changed: vec![], note: String::new() })
        }
        Request::EventsSubscribe(p) => {
            if let Some(name) = p.hold.as_deref() {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
                take_hold(&ctx.holds, name, conn.id, conn.client.as_deref().unwrap_or("unknown"), now)?;
            }
            let kinds = p.kinds.map(|k| k.into_iter().take(32).collect::<HashSet<_>>());
            #[cfg(feature = "sync")]
            chat_methods::tap(ctx);
            conn.sub = Some(Sub { rx: ctx.hub.tx.subscribe(), kinds });
            reply(&json!({ "subscribed": true, "cursor": ctx.hub.cursor() }))
        }
        Request::EventsUnsubscribe(_) => {
            conn.sub = None;
            release_holds(&ctx.holds, conn.id);
            reply(&json!({ "subscribed": false }))
        }
        Request::Events(p) => {
            #[cfg(feature = "sync")]
            chat_methods::tap(ctx);
            let kinds = p.kinds.map(|k| k.into_iter().take(32).collect::<HashSet<_>>());
            let mut out = ctx.hub.since(p.since, p.limit.unwrap_or(100).min(500) as usize, kinds.as_ref());
            out.events.retain(|e| event_visible(ctx, e));
            reply(&out)
        }
        Request::WaitFor(p) => wait_for(ctx, p).await,
        Request::AwakeSet(p) => {
            let key = p.key.trim();
            if key.is_empty() || key.chars().count() > 128 || key.chars().any(char::is_control) {
                return Err(ApiError::params("`key` must be 1-128 characters without control characters"));
            }
            match p.secs.filter(|&s| s > 0) {
                Some(secs) => {
                    let client = conn.client.clone().unwrap_or_default();
                    let who = p.who.as_deref().map(str::trim).filter(|w| !w.is_empty()).unwrap_or(&client);
                    ctx.awake.set(key, &clip(&scrub_urls(who), 64), &client, Duration::from_secs(secs));
                }
                None => {
                    ctx.awake.release(key);
                }
            }
            awake_status(ctx).await
        }
        Request::AwakeStatus(_) => awake_status(ctx).await,
        Request::UpdateStatus(_) => {
            let b = updater(ctx, NO_BUILDS).await?;
            reply(&blocking(move || b.status()).await?)
        }
        Request::UpdateInstall(p) => {
            let b = updater(ctx, NO_BUILDS).await?;
            let path = absolute(&p.path)?;
            let out = blocking(move || -> Result<UpdateStatusDto, ApiError> {
                let note = b.install(&path, p.sha256.as_deref().map(str::trim).filter(|s| !s.is_empty())).map_err(ApiError::invalid)?;
                Ok(UpdateStatusDto { note, ..b.status() })
            })
            .await??;
            reply(&out)
        }
        Request::UpdateDevBuilds(p) => {
            let b = updater(ctx, NO_BUILDS).await?;
            let out = blocking(move || -> Result<UpdateStatusDto, ApiError> {
                let note = b.set_dev_builds(p.on).map_err(ApiError::invalid)?;
                Ok(UpdateStatusDto { note, ..b.status() })
            })
            .await??;
            // The user sees in Settings › Agents what it led to (the way back to the release, say).
            let mut r = reply(&out)?;
            r.note = Some(out.note);
            Ok(r)
        }
        Request::UpdateWatch(p) => {
            let b = updater(ctx, NO_BUILDS).await?;
            let folder = p.folder.as_deref().map(absolute).transpose()?;
            let out = blocking(move || -> Result<UpdateStatusDto, ApiError> {
                let note = b.watch(folder.as_deref()).map_err(ApiError::invalid)?;
                Ok(UpdateStatusDto { note, ..b.status() })
            })
            .await??;
            reply(&out)
        }
        Request::UpdateNow(_) => {
            let b = updater(ctx, NO_BUILDS).await?;
            let out = blocking(move || -> Result<UpdateStatusDto, ApiError> {
                let note = b.update_now().map_err(ApiError::invalid)?;
                Ok(UpdateStatusDto { note, ..b.status() })
            })
            .await??;
            let mut r = reply(&out)?;
            r.note = Some(out.note);
            Ok(r)
        }
        Request::Restart(_) => {
            let b = updater(ctx, "this Seedwell cannot restart itself (the app can; a daemon is restarted by whoever started it)").await?;
            let out = blocking(move || b.restart()).await?.map_err(ApiError::invalid)?;
            let out = RestartDto { pid: std::process::id(), started_ms: ctx.started_ms, ..out };
            let mut r = reply(&out)?;
            r.note = Some(out.note.clone());
            Ok(r)
        }
        Request::AgentsRequest(p) => agents_request(ctx, conn, p).await,
        Request::AgentsRequests(p) => {
            let desk = ctx.desk.clone();
            let list = blocking(move || desk.list(p.all)).await?;
            reply(&AccessRequestsDto { permission: ctx.perm.get().as_str().into(), requests: list.iter().map(|e| e.dto()).collect() })
        }
        Request::TrackerLog(p) => {
            let st = ctx.state.clone();
            let out = blocking(move || {
                let enabled = st.config().advanced.developer_mode;
                let dir = st.dir().join("logs");
                let q = seedwell_devlog::read::Query {
                    torrent: p.torrent.clone(),
                    tracker: p.tracker.clone(),
                    event: p.event.clone(),
                    errors_only: p.errors_only.unwrap_or(false),
                    text: None,
                    limit: p.limit.unwrap_or(50).clamp(1, 500) as usize,
                };
                let since = p.since_ms.unwrap_or(0);
                let entries: Vec<Value> = seedwell_devlog::read::tail(&dir, &q)
                    .iter()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .filter(|v| v["ts_ms"].as_u64().unwrap_or(0) > since)
                    .collect();
                json!({ "enabled": enabled, "path": dir.join("trackers.log").display().to_string(), "entries": entries })
            })
            .await?;
            reply(&out)
        }
        Request::TrackerLogClear(_) => {
            ctx.backend.tracker_log_clear(&ctx.state.dir().join("logs")).await;
            reply(&json!({ "cleared": true }))
        }
        Request::ProxyStatus(_) => {
            let p = proxy(ctx)?;
            reply(&blocking(move || p.status()).await?)
        }
        Request::ProxySet(p) => {
            if p.on.is_none() && p.country.is_none() {
                return Err(ApiError::params("give on or country"));
            }
            // All or nothing: a switch that cannot be made leaves the country as it was.
            proxy_reply(ctx, move |x| {
                let before = x.status();
                if p.on.is_some_and(|on| on != before.enabled) && !before.has_key {
                    return Err("No proxy key is installed: set one up first (seedwell proxy nordvpn, or custom).".into());
                }
                let mut said = Vec::new();
                if let Some(code) = &p.country {
                    said.push(x.country(code)?);
                }
                if let Some(on) = p.on {
                    match x.switch(on) {
                        Ok(s) => said.push(s),
                        Err(e) => {
                            if p.country.is_some() {
                                let _ = x.country(&before.country);
                            }
                            return Err(e);
                        }
                    }
                }
                Ok(said.join(" "))
            })
            .await
        }
        Request::ProxyKey(p) => {
            let ProxyKeyParams { kind, user, pass, host, port } = p;
            match kind.as_str() {
                "nordvpn" => {
                    let (Some(user), Some(pass)) = (user, pass) else {
                        return Err(ApiError::params("nordvpn takes user and pass (NordVPN's service credentials)"));
                    };
                    if host.is_some() || port.is_some() {
                        return Err(ApiError::params("nordvpn uses NordVPN's own servers: no host or port"));
                    }
                    proxy_reply(ctx, move |x| x.nordvpn(user, pass)).await
                }
                "custom" => {
                    let (Some(host), Some(port)) = (host, port) else {
                        return Err(ApiError::params("custom takes host and port"));
                    };
                    let key = match (user, pass) {
                        (Some(u), Some(p)) => Some((u, p)),
                        (None, None) => None,
                        _ => return Err(ApiError::params("give both user and pass, or neither")),
                    };
                    proxy_reply(ctx, move |x| x.custom(host, port, key)).await
                }
                _ => Err(ApiError::params("kind is nordvpn or custom")),
            }
        }
        Request::ProxyRemoveKey(_) => proxy_reply(ctx, |x| x.remove_key()).await,
        Request::ProxyFaster(_) => proxy_reply(ctx, |x| x.faster()).await,
        Request::Activity(p) => {
            // What agents did in a channel closed to agents (or with private messages, while those are) is left out:
            // its entries name the channel. The user sees every entry in the app.
            let limit = p.limit.unwrap_or(50).min(1000) as usize;
            let entries: Vec<ActivityDto> = ctx.activity.recent(1000).into_iter().filter(|e| activity_visible(ctx, e)).take(limit).collect();
            reply(&json!({ "entries": entries }))
        }

        Request::Add(p) => add(ctx, p).await,
        Request::Pause(t) => bulk(ctx, t, "paused").await,
        Request::Resume(t) => bulk(ctx, t, "resumed").await,
        Request::MoveToTop(p) => {
            let picked = pick(ctx, &p.ids).await?;
            b.move_to_top(ids_of(&picked)).await?;
            action("moved to top", &picked)
        }
        Request::SetPriority(p) => {
            let picked = pick(ctx, &p.ids).await?;
            b.set_priority(ids_of(&picked), p.priority).await?;
            action(&format!("priority {}", p.priority.as_str()), &picked)
        }
        Request::SetFilePriorities(p) => set_file_priorities(ctx, p).await,
        Request::SetCategory(p) => {
            let cat = p.category.as_deref().map(ops::parse_category).transpose()?;
            let picked = pick(ctx, &p.ids).await?;
            b.set_category(ids_of(&picked), cat.clone()).await?;
            action(&format!("category {}", cat.as_deref().unwrap_or("(detect)")), &picked)
        }
        Request::SetSeedGoal(p) if p.rule_default => {
            ops::check_rule_default(&p)?;
            let picked = pick(ctx, &p.ids).await?;
            b.clear_seed_goal(ids_of(&picked)).await?;
            action("seeding goal back to the rule's", &picked)
        }
        Request::SetSeedGoal(p) => {
            let goal = ops::build_goal(&p)?;
            if p.at_goal == Some(AtGoalParam::RemoveAndDelete) {
                // Deleting files at the goal is a delayed `remove` with delete_files: it must not
                // slip past the `full` requirement.
                let level = ctx.perm.get();
                if !level.allows(Permission::Full) {
                    return Err(denied("set_seed_goal with at_goal=remove_and_delete", Permission::Full, level));
                }
            }
            let picked = pick(ctx, &p.ids).await?;
            b.set_seed_goal(ids_of(&picked), goal).await?;
            action("seeding goal set", &picked)
        }
        Request::SetLimits(p) => {
            for v in [p.down_limit, p.up_limit, p.alt_down_limit, p.alt_up_limit].into_iter().flatten() {
                if v > seedwell_state::config::MAX_RATE {
                    return Err(ApiError::params("limit above 1 TiB/s"));
                }
            }
            if p == SetLimitsParams::default() {
                return Err(ApiError::params("give at least one of down_limit, up_limit, alt_down_limit, alt_up_limit, alt_speed"));
            }
            let limits = b.set_limits(p.clone()).await?;
            persist_limits(ctx, &p).await;
            let mut r = reply(&limits)?;
            r.note = Some(format!("down={:?} up={:?} alt={}", limits.down_limit, limits.up_limit, limits.alt_active));
            Ok(r)
        }
        Request::SetTrackerLimits(p) => {
            for v in [p.down_limit, p.up_limit].into_iter().flatten() {
                if v > seedwell_state::config::MAX_RATE {
                    return Err(ApiError::params("limit above 1 TiB/s"));
                }
            }
            if p.down_limit.is_none() && p.up_limit.is_none() {
                return Err(ApiError::params("give at least one of down_limit, up_limit"));
            }
            let st = ctx.state.clone();
            let id = p.id.clone();
            let saved = blocking(move || {
                let mut set = st.profiles();
                let Some(cur) = set.get(&id) else {
                    return Err(format!("`{id}` is not a favorite tracker (Save as Favorite first)"));
                };
                let down = p.down_limit.or(cur.down_limit);
                let up = p.up_limit.or(cur.up_limit);
                set.set_limits(&id, down, up).map_err(|e| e.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))?;
                st.save_profiles(&set).map_err(|e| e.to_string())?;
                let f = set.get(&id).cloned().unwrap_or_default();
                Ok((f.down_cap(), f.up_cap()))
            })
            .await?
            .map_err(|m| ApiError::params(&m))?;
            let mut r = reply(&json!({ "id": p.id, "down_limit": saved.0, "up_limit": saved.1 }))?;
            r.note = Some(format!("{}: down={:?} up={:?}", p.id, saved.0, saved.1));
            Ok(r)
        }
        Request::SetTrackerPex(p) => {
            let (rows, _) = rows(ctx).await?;
            let st = ctx.state.clone();
            let id = p.id.trim().to_ascii_lowercase();
            let id2 = id.clone();
            let known = blocking(move || -> Result<(), String> {
                let set = st.profiles();
                // The tracker must exist (a favorite, an automatic tracker with torrents, or Other private)
                // and be private: peer exchange is a setting of private trackers only.
                let private = if set.get(&id2).is_some() {
                    rows.iter().any(|r| r.private && r.tracker_profile.as_deref() == Some(id2.as_str()))
                } else if id2 == "other-private" {
                    rows.iter().any(|r| r.private && r.tracker_profile.is_none() && seedwell_state::trackers::group_of_host(&r.tracker_host).is_none())
                } else {
                    rows.iter().any(|r| {
                        r.private && r.tracker_profile.is_none() && seedwell_state::trackers::group_of_host(&r.tracker_host).is_some_and(|g| g.key == id2)
                    })
                };
                if !private {
                    return Err(format!("`{id2}` is not a private tracker with transfers (see `trackers`); peer exchange is a setting of private trackers"));
                }
                Ok(())
            })
            .await?;
            known.map_err(|m| ApiError::params(&m))?;
            let st = ctx.state.clone();
            let allow = p.allow;
            let id3 = id.clone();
            blocking(move || -> Result<(), String> {
                let mut set = st.profiles();
                set.set_pex(&id3, allow).map_err(|e| e.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))?;
                st.save_profiles(&set).map_err(|e| e.to_string())
            })
            .await?
            .map_err(|m| ApiError::params(&m))?;
            let mut r = reply(&json!({ "id": id, "allow_pex": allow }))?;
            r.note = Some(format!("{id}: peer exchange {}", if allow { "allowed" } else { "forbidden" }));
            Ok(r)
        }
        Request::Reannounce(p) => {
            let picked = pick(ctx, &p.ids).await?;
            let mut out = b.reannounce(ids_of(&picked), p.force, p.wait).await?;
            for r in &mut out {
                for t in &mut r.trackers {
                    t.url = host_only(&t.url);
                    t.status = clip(&scrub_urls(&t.status), 300);
                }
            }
            let mut r = reply(&json!({ "transfers": out }))?;
            r.note = Some(format!("{} transfer(s)", picked.len()));
            Ok(r)
        }
        Request::Recheck(p) => {
            let picked = pick(ctx, &p.ids).await?;
            b.recheck(ids_of(&picked)).await?;
            action("recheck started", &picked)
        }
        Request::Choice(p) => {
            let picked = pick(ctx, std::slice::from_ref(&p.id)).await?;
            let mut c = b.choice(picked[0].id).await?;
            clean_choice(&mut c);
            let mut r = reply(&c)?;
            r.note = Some(clip(&c.summary, 200));
            Ok(r)
        }
        Request::ChooseFiles(p) => choose_files(ctx, p).await,
        Request::CancelChoice(p) => cancel_held(ctx, p).await,
        Request::SimAllow(p) => {
            // Only while Developer mode is on (Settings > Advanced); stopping is always allowed.
            if p.allow {
                let st = ctx.state.clone();
                let dev = blocking(move || st.config().advanced.developer_mode).await?;
                if !dev {
                    return Err(ApiError::new(ErrorCode::PermissionDenied, "simulated torrents need Developer mode: turn it on with `seedwell dev on`"));
                }
            }
            if p.ids.is_empty() || p.ids.len() > 100 {
                return Err(ApiError::params("give 1 to 100 `ids`"));
            }
            let ids: Vec<TransferId> = p.ids.iter().map(|i| parse_id(i).ok_or_else(|| ApiError::params("`ids` are 40-character info-hashes"))).collect::<Result<_, _>>()?;
            b.sim_allow(ids, p.allow).await?;
            reply(&json!({ "allowed": p.allow, "count": p.ids.len() }))
        }
        Request::DebugTrackers(p) => debug_trackers(ctx, p).await,
        Request::DebugPeers(p) => debug_peers(ctx, p).await,
        Request::DebugPerf(_) => debug_perf(ctx).await,
        Request::DebugHealth(_) => {
            let (rows, _) = rows(ctx).await?;
            let probe = b.probe().await?;
            let limits = b.limits().await?;
            // Torrents whose every tracker is halted or backing off (the first 60 that are not simply done and quiet).
            let mut trouble: Vec<(String, String, String)> = Vec::new();
            let rank = |r: &TransferDto| match r.state.as_str() {
                "downloading" | "metadata" | "queued" | "error" => 0,
                "seeding" | "low-power" => 1,
                _ => 2,
            };
            let mut look: Vec<&TransferDto> = rows.iter().filter(|r| r.kind == "torrent" && rank(r) < 2).collect();
            look.sort_by_key(|r| rank(r));
            for r in look.into_iter().take(60) {
                let Some(id) = parse_id(&r.id) else { continue };
                let Ok(d) = b.detail(id).await else { continue };
                if !d.trackers.is_empty() && d.trackers.iter().all(|t| matches!(t.state.as_str(), "halted" | "backoff")) {
                    let t = &d.trackers[0];
                    trouble.push((r.id.clone(), r.name.clone(), clip(&scrub_urls(&t.status), 160)));
                }
            }
            reply(&crate::debug::health(&probe, &rows, &limits, &trouble))
        }
        Request::LocationMoves(_) => {
            let mut m = b.location_moves().await?;
            for i in &mut m.items {
                i.name = clip(&ops::clean_name(&i.name), 200);
            }
            let mut r = reply(&m)?;
            r.note = Some(format!("{} torrent(s) out of place", m.items.len()));
            Ok(r)
        }
        Request::Move(p) => {
            if p.ids.is_empty() {
                return Err(ApiError::params("give `ids`"));
            }
            // A move decides where bytes land, like a custom save folder: `full` (the method's
            // level) and never into system, Library or hidden folders.
            let path = match p.path.as_deref() {
                Some(raw) => {
                    let path = ops::clean_path(raw, "path")?;
                    if ops::is_sensitive_dir(&path) {
                        return Err(ApiError::invalid("`path` points at a system, Library or hidden folder; choose a normal folder"));
                    }
                    Some(path)
                }
                None => None,
            };
            let picked = pick(ctx, &p.ids).await?;
            let (queued, refused) = b.move_to(ids_of(&picked), path.clone()).await?;
            let name_of = |id: &TransferId| picked.iter().find(|p| p.id == *id).map(|p| p.name.clone()).unwrap_or_default();
            let rep = MoveReportDto {
                queued: queued.iter().map(|id| IdName { id: id_hex(id), name: clip(&ops::clean_name(&name_of(id)), 200) }).collect(),
                refused: refused.iter().map(|(id, why)| MoveRefused { id: id_hex(id), reason: clip(why, 200) }).collect(),
            };
            let mut r = reply(&rep)?;
            r.note = Some(format!(
                "move {} to {}",
                names_line(&picked),
                path.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "where the settings say".into())
            ));
            Ok(r)
        }
        Request::CancelMove(p) => {
            let picked = pick(ctx, &p.ids).await?;
            b.cancel_moves(ids_of(&picked)).await?;
            action("moves cancelled", &picked)
        }
        Request::StreamUrl(p) => {
            let picked = pick(ctx, std::slice::from_ref(&p.id)).await?;
            let mut dto = b.stream_url(picked[0].id, p.file).await?;
            dto.id = id_hex(&picked[0].id);
            dto.file_name = clip(&dto.file_name, 200);
            let mut r = reply(&dto)?;
            r.note = Some(format!("stream {} file {}", clip(&picked[0].name, 60), dto.file));
            Ok(r)
        }
        Request::StopStream(p) => {
            let closed = b.stop_stream(&p.token).await?;
            let mut r = reply(&serde_json::json!({ "closed": closed }))?;
            r.note = Some("stream stopped".into());
            Ok(r)
        }
        Request::Remove(p) => remove_plan(ctx, conn, p).await,
        Request::RemoveConfirm(p) => remove_confirm(ctx, conn, p).await,
        r @ (Request::SyncList(_)
        | Request::SyncStatus(_)
        | Request::SyncAdd(_)
        | Request::SyncJoin(_)
        | Request::SyncShare(_)
        | Request::SyncMembers(_)
        | Request::SyncInvites(_)
        | Request::SyncRevokeInvite(_)
        | Request::SyncSetRole(_)
        | Request::SyncRemoveDevice(_)
        | Request::SyncReshare(_)
        | Request::SyncSetShareUntil(_)
        | Request::SyncRename(_)
        | Request::SyncMove(_)
        | Request::SyncPause(_)
        | Request::SyncResume(_)
        | Request::SyncRescan(_)
        | Request::SyncSetLanOnly(_)
        | Request::SyncReadOnly(_)
        | Request::SyncSetReadOnly(_)
        | Request::SyncConflicts(_)
        | Request::SyncResolve(_)
        | Request::SyncAddAddress(_)
        | Request::SyncRemove(_)
        | Request::SyncRemoveConfirm(_)
        | Request::SyncSettings(_)
        | Request::SyncSetSettings(_)
        | Request::SyncSetMode(_)
        | Request::SyncItems(_)
        | Request::SyncUnavailable(_)
        | Request::SyncChanges(_)
        | Request::SyncFetch(_)
        | Request::SyncCancelFetch(_)
        | Request::SyncPin(_)
        | Request::SyncUnpin(_)
        | Request::SyncEvict(_)
        | Request::SyncHostWrite(_)
        | Request::SyncHostMkdir(_)
        | Request::SyncHostDelete(_)
        | Request::SyncHostRename(_)) => sync_call(ctx, conn, r).await,
        r @ (Request::ChatList(_)
        | Request::ChatNew(_)
        | Request::ChatJoin(_)
        | Request::ChatShare(_)
        | Request::ChatMembers(_)
        | Request::ChatSetRole(_)
        | Request::ChatRemoveDevice(_)
        | Request::ChatSetShareUntil(_)
        | Request::ChatSend(_)
        | Request::ChatRead(_)
        | Request::ChatAttachment(_)
        | Request::ChatDownload(_)
        | Request::ChatCancelDownload(_)
        | Request::ChatStopSharing(_)
        | Request::ChatMute(_)
        | Request::ChatRename(_)
        | Request::ChatGroups(_)
        | Request::ChatGroupNew(_)
        | Request::ChatGroupEdit(_)
        | Request::ChatGroupDelete(_)
        | Request::ChatSetGroup(_)
        | Request::ChatAddAddress(_)
        | Request::ChatSetWake(_)
        | Request::ChatSetAgents(_)
        | Request::ChatLeave(_)
        | Request::ChatTopics(_)
        | Request::ChatNewTopic(_)
        | Request::ChatDmList(_)
        | Request::ChatDmRead(_)
        | Request::ChatDmSend(_)
        | Request::ChatDmDelete(_)) => chat_call(ctx, conn, r).await,
        Request::SettingsSet(p) => {
            let st = ctx.state.clone();
            let SettingsSetParams { changes, file, doc } = p;
            let changed = blocking(move || {
                let save_err = |e: seedwell_state::DocError| ApiError::invalid(clip(&e.to_string(), 300));
                match (file, doc) {
                    (None, None) => {
                        let cfg = st.config();
                        let (new, changed) = crate::settings::apply(&cfg, &changes)?;
                        if !changed.is_empty() {
                            st.save_config(&new).map_err(save_err)?;
                        }
                        Ok::<_, ApiError>(changed)
                    }
                    (Some(file), Some(doc)) if changes.is_empty() => {
                        let same = match crate::settings::check_file(&file)? {
                            "rules" => {
                                let old = st.rules();
                                let new = crate::settings::set_rules(&old, doc)?;
                                let same = new == old;
                                if !same {
                                    st.save_rules(&new).map_err(save_err)?;
                                }
                                same
                            }
                            _ => {
                                let old = st.profiles();
                                let new = crate::settings::set_trackers(&old, doc)?;
                                let same = new == old;
                                if !same {
                                    st.save_profiles(&new).map_err(save_err)?;
                                }
                                same
                            }
                        };
                        Ok(if same { Vec::new() } else { vec![format!("{file}.toml")] })
                    }
                    _ => Err(ApiError::params("give changes, or a file (rules or trackers) with its doc")),
                }
            })
            .await??;
            let nothing = changed.is_empty();
            let mut r = reply(&SettingsDto {
                settings: Default::default(),
                changed: changed.clone(),
                note: if nothing { "nothing changed: every value given was already in force".into() } else { "saved; the running app applies settings within a few seconds".into() },
            })?;
            // The activity log names what really changed, never what was only asked.
            r.note = Some(if nothing { "nothing changed".into() } else { changed.join(", ") });
            Ok(r)
        }
    }
}

#[cfg(feature = "sync")]
#[path = "server_sync.rs"]
mod sync_methods;

#[cfg(feature = "sync")]
async fn sync_call<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, r: Request) -> Result<Reply, ApiError> {
    sync_methods::call(ctx, conn, r).await
}

#[cfg(feature = "sync")]
#[path = "server_chat.rs"]
mod chat_methods;

#[cfg(feature = "sync")]
async fn chat_call<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, r: Request) -> Result<Reply, ApiError> {
    chat_methods::call(ctx, conn, r).await
}

/// Built without folder sync: every `chat.*` method says so.
#[cfg(not(feature = "sync"))]
async fn chat_call<B: Backend>(_ctx: &Arc<Ctx<B>>, _conn: &Conn, _r: Request) -> Result<Reply, ApiError> {
    Err(ApiError::new(ErrorCode::Unsupported, "channels are not available (this build has no folder sync)"))
}

/// Built without folder sync: every `sync.*` method says so.
#[cfg(not(feature = "sync"))]
async fn sync_call<B: Backend>(_ctx: &Arc<Ctx<B>>, _conn: &Conn, _r: Request) -> Result<Reply, ApiError> {
    Err(ApiError::new(ErrorCode::Unsupported, "folder sync is not running (this build has no folder sync)"))
}

/// The name an agent's request goes under: the one it gives (its project's), else its client name.
fn agent_label(conn: &Conn, given: Option<&str>) -> Result<String, ApiError> {
    let raw = given.or(conn.client.as_deref()).unwrap_or("agent");
    let name: String = raw.chars().filter(|c| !c.is_control()).collect();
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(ApiError::params("`agent` is a name of 1 to 64 characters"));
    }
    Ok(name.to_owned())
}

/// `agents.request`: records the request (or finds the one pending for the same thing), announces it in a channel
/// when asked to, and says what happens now. Nothing is granted here.
async fn agents_request<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, p: AgentsRequestParams) -> Result<Reply, ApiError> {
    use crate::access::Want;
    let want = Want::parse(&p.what, p.target.as_deref())?;
    let agent = agent_label(conn, p.agent.as_deref())?;
    if p.channel.is_none() && (p.topic.is_some() || p.reply_to.is_some()) {
        return Err(ApiError::params("`topic` and `reply_to` go with `channel`"));
    }
    // The channel to announce in first: a wrong one asks nothing.
    #[cfg(feature = "sync")]
    let place = match p.channel.as_deref() {
        Some(c) => {
            let place = chat_methods::announce_place(ctx, c, p.topic.as_deref())?;
            // The replies to the announcement reach the desk through the pump: a receiver made now misses none (the
            // pump finds a service that just started only a moment later).
            chat_methods::tap(ctx);
            Some(place)
        }
        None => None,
    };
    #[cfg(not(feature = "sync"))]
    if p.channel.is_some() {
        return Err(ApiError::new(ErrorCode::Unsupported, "channels are not available (this build has no folder sync)"));
    }
    let desk = ctx.desk.clone();
    let why = p.why.clone();
    let (who, w) = (agent.clone(), want.clone());
    let (e, dup) = blocking(move || desk.ask(w, &who, why.as_deref())).await??;
    #[cfg_attr(not(feature = "sync"), allow(unused_mut))]
    let mut said = Vec::new();
    #[cfg(feature = "sync")]
    let mut e = e;
    #[cfg(feature = "sync")]
    if let Some(place) = place
        && e.announced.is_none()
    {
        match chat_methods::announce(ctx, &place, &e, p.reply_to.as_deref()).await {
            Ok((channel, message)) => {
                ctx.desk.announced(&e.id, &channel, &message);
                e.announced = Some(crate::access::Announcement { channel, message });
                said.push("announced it in the channel: the user's reply \"yes\" or \"no\" there answers it too".to_owned());
            }
            Err(err) => said.push(format!("it could not be announced in the channel ({})", clip(&err.message, 200))),
        }
    }
    let head = if dup {
        format!("The user was already asked for {} ({}); you are added to that request", want.text(), e.who())
    } else {
        format!("Asked the user for {}", want.text())
    };
    let tail = "Nothing changes until the user answers in Seedwell (a notification with Approve and Deny, Settings › Agents) or with a reply in chat. `seedwell agents requests` shows the answer (also an `agents.request` event); unanswered, it expires in a day";
    let note = std::iter::once(head).chain(said).chain(std::iter::once(tail.to_owned())).collect::<Vec<_>>().join("; ");
    let mut r = reply(&AccessAskedDto { request: e.dto(), duplicate: dup, note })?;
    r.note = Some(format!("request {}{}", e.id, if dup { " (asked already)" } else { "" }));
    Ok(r)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|_| ApiError::internal("background task failed"))
}

async fn pick<B: Backend>(ctx: &Ctx<B>, ids: &[String]) -> Result<Vec<Picked>, ApiError> {
    let (rows, _) = rows(ctx).await?;
    ops::resolve_ids(&rows, ids)
}

async fn bulk<B: Backend>(ctx: &Arc<Ctx<B>>, t: Targets, what: &str) -> Result<Reply, ApiError> {
    if t.all == !t.ids.is_empty() {
        return Err(ApiError::params("give either `ids` or `all: true`"));
    }
    let resume = what == "resumed";
    if t.all {
        if resume {
            ctx.backend.resume_all().await?;
        } else {
            ctx.backend.pause_all().await?;
        }
        let (rows, _) = rows(ctx).await?;
        let mut r = reply(&json!({ "action": what, "all": true, "count": rows.len() }))?;
        r.note = Some(format!("{what} all"));
        return Ok(r);
    }
    let picked = pick(ctx, &t.ids).await?;
    if resume {
        ctx.backend.resume(ids_of(&picked)).await?;
    } else {
        ctx.backend.pause(ids_of(&picked)).await?;
    }
    action(what, &picked)
}

fn names_line(p: &[Picked]) -> String {
    p.iter().take(5).map(|p| clip(&p.name, 60)).collect::<Vec<_>>().join(", ")
}

/// File labels come from the torrent: strip control and bidi characters, cap the length.
fn clean_choice(c: &mut ChoiceDto) {
    fn plain(s: &str) -> String {
        s.chars()
            .filter(|c| !c.is_control() && !matches!(*c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
            .collect()
    }
    for g in &mut c.groups {
        g.label = clip(&ops::clean_name(&plain(&g.label)), 200);
        for i in &mut g.items {
            i.label = clip(&ops::clean_name(&plain(&i.label)), 300);
        }
    }
}

async fn choose_files<B: Backend>(ctx: &Arc<Ctx<B>>, p: ChooseFilesParams) -> Result<Reply, ApiError> {
    let given = u8::from(p.accept_default) + u8::from(p.files.is_some()) + u8::from(p.skip.is_some());
    if given != 1 {
        return Err(ApiError::params("give exactly one of `accept_default`, `files` (download only these) or `skip` (download all but these)"));
    }
    let picked = pick(ctx, std::slice::from_ref(&p.id)).await?;
    let pending = state_of(ctx, &picked[0]).await? == "pending";
    let prios = if p.accept_default {
        None
    } else {
        let detail = ctx.backend.detail(picked[0].id).await?;
        let n = detail.files.len();
        if n == 0 {
            return Err(ApiError::invalid("this transfer has no files yet (a magnet waits for its metadata)"));
        }
        let (list, listed_wanted) = match (&p.files, &p.skip) {
            (Some(f), _) => (f, true),
            (_, Some(s)) => (s, false),
            _ => unreachable!("exactly one is given"),
        };
        if let Some(bad) = list.iter().find(|i| **i as usize >= n) {
            return Err(ApiError::params(format!("file index {bad} is out of range (0..{n})")));
        }
        let set: HashSet<u32> = list.iter().copied().collect();
        Some(
            detail
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let listed = set.contains(&(i as u32));
                    if f.padding || listed == listed_wanted { FilePriorityParam::Normal } else { FilePriorityParam::Skip }
                })
                .collect::<Vec<_>>(),
        )
    };
    let skipped = prios.as_ref().map(|v| v.iter().filter(|p| **p == FilePriorityParam::Skip).count());
    if pending {
        // A torrent in its grace period is not waiting for a choice: apply the files, then start it now.
        if let Some(v) = prios {
            ctx.backend.set_file_priorities(picked[0].id, v).await?;
        }
        ctx.backend.start_now(picked[0].id).await?;
    } else {
        ctx.backend.choose_files(picked[0].id, prios).await?;
    }
    let mut r = action("started", &picked)?;
    r.note = Some(match skipped {
        Some(k) => format!("{k} files skipped on {}", clip(&picked[0].name, 60)),
        None => format!("default choice on {}", clip(&picked[0].name, 60)),
    });
    Ok(r)
}

async fn set_file_priorities<B: Backend>(ctx: &Arc<Ctx<B>>, p: SetFilePrioritiesParams) -> Result<Reply, ApiError> {
    let picked = pick(ctx, std::slice::from_ref(&p.id)).await?;
    let detail = ctx.backend.detail(picked[0].id).await?;
    let n = detail.files.len();
    if n == 0 {
        return Err(ApiError::invalid("this transfer has no files (no metadata yet, or a direct download)"));
    }
    let full: Vec<FilePriorityParam> = match (p.priorities, p.set) {
        (Some(_), Some(_)) => return Err(ApiError::params("give either `priorities` or `set`, not both")),
        (None, None) => return Err(ApiError::params("give `priorities` (one per file) or `set` (index + priority)")),
        (Some(v), None) => {
            if v.len() != n {
                return Err(ApiError::params(format!("`priorities` has {} entries but the torrent has {n} files", v.len())));
            }
            v
        }
        (None, Some(sets)) => {
            let mut cur: Vec<FilePriorityParam> = detail
                .files
                .iter()
                .map(|f| match f.priority.as_str() {
                    "skip" => FilePriorityParam::Skip,
                    "low" => FilePriorityParam::Low,
                    "high" => FilePriorityParam::High,
                    _ => FilePriorityParam::Normal,
                })
                .collect();
            for s in sets {
                let Some(slot) = cur.get_mut(s.index as usize) else {
                    return Err(ApiError::params(format!("file index {} is out of range (0..{n})", s.index)));
                };
                *slot = s.priority;
            }
            cur
        }
    };
    let skipped = full.iter().filter(|p| **p == FilePriorityParam::Skip).count();
    ctx.backend.set_file_priorities(picked[0].id, full).await?;
    let mut r = action("file priorities set", &picked)?;
    r.note = Some(format!("{} files, {skipped} skipped, on {}", n, clip(&picked[0].name, 60)));
    Ok(r)
}

async fn add<B: Backend>(ctx: &Arc<Ctx<B>>, p: AddParams) -> Result<Reply, ApiError> {
    let kind = ops::pick_source(&p)?;
    let source = match kind {
        SourceKind::Magnet(m) => AddSource::Magnet(m),
        SourceKind::Url(u) => AddSource::Url(u),
        SourceKind::Bytes(b) => AddSource::TorrentFile(b),
        SourceKind::Path(path) => AddSource::TorrentFile(read_torrent_file(&path).await?),
    };
    if p.sha256.is_some() && !matches!(source, AddSource::Url(_)) {
        return Err(ApiError::params("`sha256` applies to direct downloads (`url`) only"));
    }
    if let Some(rule) = &p.rule {
        let st = ctx.state.clone();
        let rule2 = rule.clone();
        let known = blocking(move || st.rules().get(&rule2).is_some()).await?;
        if !known {
            return Err(ApiError::invalid(format!("no rule with id `{}`; see the `rules` method", clip(rule, 64))));
        }
    }
    let args = AddArgs {
        source,
        save_path: expand_save_path(&p.save_path, ctx.perm.get())?,
        paused: p.paused,
        category: p.category.as_deref().map(ops::parse_category).transpose()?,
        priority: p.priority,
        rule: p.rule.clone(),
        queue_top: p.queue_top,
        start_now: p.now,
        sha256: p.sha256.as_deref().map(ops::parse_sha256).transpose()?,
        peers: ops::parse_peers(&p.peers)?,
    };
    let mut added = ctx.backend.add(args).await?;
    added.name = clip(&ops::clean_name(&added.name), 300);
    if let Some(c) = &mut added.choice {
        clean_choice(c);
    }
    let mut r = reply(&added)?;
    r.note = Some(format!("{} {}", short(&added.id), clip(&added.name, 80)));
    Ok(r)
}

async fn read_torrent_file(path: &Path) -> Result<Vec<u8>, ApiError> {
    let fail = |what: &str| ApiError::invalid(format!("cannot read `{}`: {what}", clip(&path.display().to_string(), 200)));
    let meta = tokio::fs::metadata(path).await.map_err(|e| fail(&e.kind().to_string()))?;
    if !meta.is_file() {
        return Err(fail("not a regular file"));
    }
    if meta.len() == 0 || meta.len() > ops::MAX_TORRENT_BYTES as u64 {
        return Err(fail("empty or larger than 16 MiB"));
    }
    tokio::fs::read(path).await.map_err(|e| fail(&e.kind().to_string()))
}

async fn persist_limits<B: Backend>(ctx: &Arc<Ctx<B>>, p: &SetLimitsParams) {
    // Limits are a Control-level action; they are also written to config.toml so the Settings
    // window and the next launch agree with what the agent set. Best effort: the engine already
    // applied them.
    let st = ctx.state.clone();
    let p = p.clone();
    let _ = blocking(move || {
        let mut cfg = st.config();
        if let Some(v) = p.down_limit {
            cfg.transfers.download_limit = v;
        }
        if let Some(v) = p.up_limit {
            cfg.transfers.upload_limit = v;
        }
        if let Some(v) = p.alt_down_limit {
            cfg.transfers.alt.download_limit = v;
        }
        if let Some(v) = p.alt_up_limit {
            cfg.transfers.alt.upload_limit = v;
        }
        if let Some(v) = p.alt_speed {
            cfg.transfers.alt.enabled = v;
        }
        let _ = st.save_config(&cfg);
    })
    .await;
}

async fn remove_plan<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, p: RemoveParams) -> Result<Reply, ApiError> {
    let level = ctx.perm.get();
    if p.delete_files && !level.allows(Permission::Full) {
        return Err(denied("remove with delete_files", Permission::Full, level));
    }
    let (rows, _) = rows(ctx).await?;
    let picked = ops::resolve_ids(&rows, &p.ids)?;
    let items: Vec<RemoveItem> = picked
        .iter()
        .filter_map(|pk| {
            let hex = id_hex(&pk.id);
            rows.iter().find(|r| r.id == hex).map(|r| RemoveItem {
                id: r.id.clone(),
                name: clip(&ops::clean_name(&r.name), 200),
                kind: r.kind.clone(),
                bytes_on_disk: r.done_bytes,
                path: r.save_path.clone(),
                state: r.state.clone(),
            })
        })
        .collect();
    let total: u64 = items.iter().map(|i| i.bytes_on_disk).sum();
    let client = conn.client.clone().unwrap_or_default();
    let token = {
        let mut c = ctx.confirms.lock().unwrap_or_else(|p| p.into_inner());
        c.issue(Instant::now(), Plan { ids: ids_of(&picked), delete_files: p.delete_files, client })
    };
    let ttl = ctx.opts.confirm_ttl.as_secs();
    let what = if items.len() == 1 { format!("`{}`", items[0].name) } else { format!("{} transfers", items.len()) };
    let summary = if p.delete_files {
        format!(
            "Will remove {what} AND DELETE {} of downloaded files from disk. Confirmation is valid for {ttl} s.",
            crate::human::bytes(total)
        )
    } else {
        format!("Will remove {what} from Seedwell; downloaded files are kept on disk. Confirmation is valid for {ttl} s.")
    };
    let mut r = reply(&RemovePlanDto { token, expires_in_secs: ttl, delete_files: p.delete_files, items, summary })?;
    r.note = Some(format!("plan for {} transfer(s), delete_files={}", picked.len(), p.delete_files));
    Ok(r)
}

async fn remove_confirm<B: Backend>(ctx: &Arc<Ctx<B>>, conn: &Conn, p: RemoveConfirmParams) -> Result<Reply, ApiError> {
    let client = conn.client.clone().unwrap_or_default();
    let plan = {
        let mut c = ctx.confirms.lock().unwrap_or_else(|p| p.into_inner());
        c.redeem(Instant::now(), p.token.trim(), &client)
    };
    let plan = plan.map_err(|e| match e {
        TokenError::Unknown => ApiError::new(
            ErrorCode::InvalidToken,
            "unknown confirmation token (wrong, already used, or issued to another client); call `remove` again",
        ),
        TokenError::Expired => ApiError::new(
            ErrorCode::TokenExpired,
            format!("the confirmation expired after {} s; call `remove` again", ctx.opts.confirm_ttl.as_secs()),
        ),
    })?;
    // The level may have dropped since step 1.
    let level = ctx.perm.get();
    if plan.delete_files && !level.allows(Permission::Full) {
        return Err(denied("remove_confirm with delete_files", Permission::Full, level));
    }
    let (rows, _) = rows(ctx).await?;
    let mut present = Vec::new();
    let mut removed = Vec::new();
    let mut missing = Vec::new();
    for id in &plan.ids {
        let hex = id_hex(id);
        match rows.iter().find(|r| r.id == hex) {
            Some(r) => {
                present.push(*id);
                removed.push(IdName { id: hex, name: clip(&ops::clean_name(&r.name), 200) });
            }
            None => missing.push(hex),
        }
    }
    if !present.is_empty() {
        ctx.backend.remove(present, plan.delete_files).await?;
    }
    let mut r = reply(&RemovedDto { removed: removed.clone(), missing, files_deleted: plan.delete_files })?;
    r.note = Some(format!(
        "removed {}{}",
        removed.iter().take(5).map(|x| clip(&x.name, 60)).collect::<Vec<_>>().join(", "),
        if plan.delete_files { " and deleted files" } else { " (files kept)" }
    ));
    Ok(r)
}

async fn trackers<B: Backend>(ctx: &Arc<Ctx<B>>) -> Result<Reply, ApiError> {
    let (rows, _) = rows(ctx).await?;
    let st = ctx.state.clone();
    let (profiles, pex_enabled) = blocking(move || (st.profiles(), st.config().network.pex)).await?;
    let mut out = TrackersDto { pex_enabled, other_private_allow_pex: profiles.allows_pex(None, None), ..Default::default() };
    for p in &profiles.profiles {
        let mine = rows.iter().filter(|r| r.tracker_profile.as_deref() == Some(p.id.as_str()));
        let mut d = ProfileDto {
            favorite: true,
            id: p.id.clone(),
            name: p.name.clone(),
            urls: p.urls.iter().map(|u| u.as_str().to_owned()).collect(),
            rule: p.rule.clone(),
            down_limit: p.down_cap(),
            up_limit: p.up_cap(),
            allow_pex: p.allow_pex,
            ..Default::default()
        };
        for r in mine {
            d.private |= r.private;
            d.transfers += 1;
            d.sharing += u32::from(r.is_sharing());
            d.up_rate += r.up_rate;
            d.down_rate += r.down_rate;
        }
        out.profiles.push(d);
    }
    for r in rows.iter().filter(|r| r.tracker_profile.is_none() && r.kind == "torrent") {
        match seedwell_state::trackers::group_of_host(&r.tracker_host) {
            Some(g) => {
                let i = match out.groups.iter().position(|d| d.id == g.key) {
                    Some(i) => i,
                    None => {
                        let allow_pex = profiles.allows_pex(None, Some(&g.key));
                        out.groups.push(ProfileDto { id: g.key, name: g.name, allow_pex, ..Default::default() });
                        out.groups.len() - 1
                    }
                };
                let d = &mut out.groups[i];
                if !d.urls.contains(&r.tracker_host) && d.urls.len() < 32 {
                    d.urls.push(r.tracker_host.clone());
                }
                d.private |= r.private;
                d.transfers += 1;
                d.sharing += u32::from(r.is_sharing());
                d.up_rate += r.up_rate;
                d.down_rate += r.down_rate;
            }
            None if r.private => out.other_private_transfers += 1,
            None => out.public_transfers += 1,
        }
    }
    out.groups.sort_by(|a, b| b.transfers.cmp(&a.transfers).then_with(|| a.name.cmp(&b.name)));
    reply(&out)
}

fn bucket_dto(b: &seedwell_state::StatsBucket) -> BucketDto {
    BucketDto {
        downloaded: b.downloaded,
        uploaded: b.uploaded,
        ratio: b.ratio(),
        torrents_added: b.torrents_added,
        torrents_completed: b.torrents_completed,
        torrents_removed: b.torrents_removed,
        files_completed: b.files_completed,
        seed_secs: b.seed_secs,
    }
}

async fn tracker_stats<B: Backend>(ctx: &Arc<Ctx<B>>, p: TrackerStatsParams) -> Result<Reply, ApiError> {
    use seedwell_state::Day;
    let name = p.tracker.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("all").to_ascii_lowercase();
    let days = p.days.unwrap_or(30).clamp(1, 400);
    let st = ctx.state.clone();
    let name2 = name.clone();
    let out = blocking(move || -> Result<TrackerStatsDto, ApiError> {
        let name2 = name2.replace("other_private", "other-private");
        let profiles = st.profiles();
        let scopes = seedwell_state::stats::scopes_for(&profiles, &name2);
        let special = matches!(name2.as_str(), "all" | "public" | "other-private");
        if !special && profiles.get(&name2).is_none() {
            // An automatic tracker is named by its domain and exists while it has history.
            let known = st.with_store(|s| s.stats_scopes()).unwrap_or_default();
            if !known.iter().any(|k| scopes.iter().any(|sc| sc.key() == Some(k.as_str()))) {
                return Err(ApiError::not_found(format!("no tracker `{}`; see the `trackers` method", clip(&name2, 64))));
            }
        }
        let today = Day::from_unix(now_secs() as i64, st.tz_offset());
        let (summary, series) = st
            .with_store(|s| {
                let mut sum = seedwell_state::StatsSummary::default();
                let mut series: Vec<(Day, seedwell_state::StatsBucket)> = Vec::new();
                for sc in &scopes {
                    sum.add(&s.stats_summary(sc, today)?);
                    for (i, (d, b)) in s.stats_series(sc, today, days)?.into_iter().enumerate() {
                        match series.get_mut(i) {
                            Some(o) => o.1.add(&b),
                            None => series.push((d, b)),
                        }
                    }
                }
                Ok((sum, series))
            })
            .map_err(|m| ApiError::new(ErrorCode::Unsupported, m))?;
        let month = &summary.this_month;
        let text = format!(
            "{}: this month {} up / {} down; all time {} up / {} down",
            name2,
            crate::human::bytes(month.uploaded),
            crate::human::bytes(month.downloaded),
            crate::human::bytes(summary.all_time.uploaded),
            crate::human::bytes(summary.all_time.downloaded)
        );
        Ok(TrackerStatsDto {
            tracker: name2,
            today: bucket_dto(&summary.today),
            this_week: bucket_dto(&summary.this_week),
            this_month: bucket_dto(&summary.this_month),
            all_time: bucket_dto(&summary.all_time),
            series: series.iter().map(|(d, b)| DayDto { date: d.iso(), bucket: bucket_dto(b) }).collect(),
            summary: text,
        })
    })
    .await??;
    reply(&out)
}

async fn wait_for<B: Backend>(ctx: &Arc<Ctx<B>>, p: WaitForParams) -> Result<Reply, ApiError> {
    let (rows0, _) = rows(ctx).await?;
    let picked = ops::resolve_ids(&rows0, &p.ids)?;
    let mode = p.mode.unwrap_or(WaitMode::All);
    let deadline = Instant::now() + Duration::from_secs(u64::from(p.timeout_secs.unwrap_or(60).min(300)));
    let mut stop = ctx.stop.clone();
    loop {
        let (rows, _) = rows(ctx).await?;
        let mine: Vec<TransferDto> = picked
            .iter()
            .filter_map(|pk| {
                let hex = id_hex(&pk.id);
                rows.iter().find(|r| r.id == hex).cloned()
            })
            .collect();
        // A transfer that vanished (removed) or failed will never finish: stop waiting for it.
        let settled = |t: &TransferDto| t.is_done() || matches!(t.state.as_str(), "error" | "missing-data");
        let vanished = picked.len() - mine.len();
        let n_settled = mine.iter().filter(|t| settled(t)).count() + vanished;
        let done = match mode {
            WaitMode::All => n_settled == picked.len(),
            WaitMode::Any => n_settled > 0,
        };
        let timed_out = !done && Instant::now() >= deadline;
        if done || timed_out {
            let mut transfers = mine;
            transfers.iter_mut().for_each(ops::sanitize_transfer);
            return reply(&WaitDto { done, timed_out, transfers });
        }
        tokio::select! {
            _ = stop.changed() => return Err(ApiError::new(ErrorCode::ShuttingDown, "server is shutting down")),
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}


/// The state name of a picked transfer right now.
async fn state_of<B: Backend>(ctx: &Ctx<B>, p: &Picked) -> Result<String, ApiError> {
    let (rows, _) = rows(ctx).await?;
    let hex = id_hex(&p.id);
    Ok(rows.iter().find(|r| r.id == hex).map(|r| r.state.clone()).unwrap_or_default())
}

/// `cancel_choice`: drop a held torrent (awaiting a choice or in its grace period). The `.torrent` file
/// goes to the Trash unless `keep_file`, as Cancel does in the app.
async fn cancel_held<B: Backend>(ctx: &Arc<Ctx<B>>, p: CancelChoiceParams) -> Result<Reply, ApiError> {
    let picked = pick(ctx, std::slice::from_ref(&p.id)).await?;
    let state = state_of(ctx, &picked[0]).await?;
    let trashed = ctx.backend.cancel_held(picked[0].id, state == "pending", p.keep_file).await?;
    let mut r = action("cancelled", &picked)?;
    if let Some(obj) = r.value.as_object_mut() {
        obj.insert("torrent_file".into(), json!(if p.keep_file { "kept" } else if trashed.is_some() { "trashed" } else { "none" }));
        if let Some(n) = &trashed {
            obj.insert("trashed_file".into(), json!(clip(n, 200)));
        }
    }
    Ok(r)
}

const DEBUG_MAX_TORRENTS: usize = 100;

async fn debug_trackers<B: Backend>(ctx: &Arc<Ctx<B>>, p: DebugTrackersParams) -> Result<Reply, ApiError> {
    let (rows, _) = rows(ctx).await?;
    let probe = ctx.backend.probe().await?;
    let now = now_ms() / 1000;
    let wanted: Vec<&TransferDto> = match &p.id {
        Some(id) => {
            let picked = ops::resolve_ids(&rows, std::slice::from_ref(id))?;
            let hex = id_hex(&picked[0].id);
            rows.iter().filter(|r| r.id == hex).collect()
        }
        None => {
            // Active ones first: trouble and downloads before the long tail of seeds.
            let rank = |r: &TransferDto| match r.state.as_str() {
                "error" | "missing-data" => 0,
                "downloading" | "metadata" | "queued" => 1,
                "seeding" | "low-power" => 2,
                _ => 3,
            };
            let mut v: Vec<&TransferDto> = rows.iter().filter(|r| r.kind == "torrent").collect();
            v.sort_by_key(|r| rank(r));
            v
        }
    };
    let needle = p.tracker.as_deref().map(str::to_ascii_lowercase);
    let mut out = Vec::new();
    let mut seen = 0usize;
    for r in wanted {
        if p.id.is_none() && out.len() >= DEBUG_MAX_TORRENTS {
            seen += 1;
            continue;
        }
        let Some(hex) = parse_id(&r.id) else { continue };
        let mut d = ctx.backend.detail(hex).await?;
        let mut trackers = Vec::new();
        for t in &mut d.trackers {
            t.url = host_only(&t.url);
            t.status = clip(&scrub_urls(&t.status), 300);
            if needle.as_deref().is_some_and(|n| !t.url.to_ascii_lowercase().contains(n)) {
                continue;
            }
            trackers.push(DebugTrackerDto {
                url: t.url.clone(),
                tier: t.tier,
                state: t.state.clone(),
                status: t.status.clone(),
                failures: t.failures,
                last_announce_at: t.last_announce_at,
                last_success_at: t.last_success_at,
                seeders: t.seeds,
                leechers: t.leechers,
                next_announce_at: t.next_announce_at,
                next_reason: crate::debug::next_reason(t, &r.state, now),
                manual_ok_at: t.manual_ok_at,
                tripped: t.tripped.clone(),
            });
        }
        if needle.is_some() && trackers.is_empty() {
            continue;
        }
        out.push(DebugTorrentTrackers { id: r.id.clone(), name: clip(&ops::clean_name(&r.name), 200), state: r.state.clone(), trackers });
    }
    let n = out.len();
    let mut r = reply(&DebugTrackersDto { online: probe.online, offline_since: probe.offline_since, transfers: out, truncated: seen as u32 })?;
    r.note = Some(format!("{n} torrent(s)"));
    Ok(r)
}

async fn debug_peers<B: Backend>(ctx: &Arc<Ctx<B>>, p: DebugPeersParams) -> Result<Reply, ApiError> {
    let (rows, _) = rows(ctx).await?;
    let picked = ops::resolve_ids(&rows, std::slice::from_ref(&p.id))?;
    let hex = id_hex(&picked[0].id);
    let row = rows.iter().find(|r| r.id == hex).cloned().unwrap_or_default();
    let d = ctx.backend.detail(picked[0].id).await?;
    let probe = ctx.backend.probe().await?;
    let total = d.peers.len();
    let peers: Vec<DebugPeerDto> = d
        .peers
        .iter()
        .take(500)
        .map(|x| DebugPeerDto {
            addr: if p.redact { crate::debug::redact_addr(&x.addr) } else { x.addr.clone() },
            client: clip(&ops::clean_name(&x.client), 40),
            transport: if x.utp { "utp" } else { "tcp" }.into(),
            direction: if x.outgoing { "out" } else { "in" }.into(),
            choking_us: x.choking_us,
            choked_by_us: x.choked_by_us,
            interested: x.interested,
            down_rate: x.down_rate,
            up_rate: x.up_rate,
            requests_in_flight: x.requests_in_flight,
            progress: x.progress,
            connected_secs: x.connected_secs,
            snubbed: x.snubbed,
            upload_only: x.upload_only,
            has_pex: x.has_pex,
            requests_received: x.requests_received,
            blocks_served: x.blocks_served,
            requests_rejected: x.requests_rejected,
            last_request_secs: x.last_request_secs,
        })
        .collect();
    let c = |k: &str| probe.counters.get(k).copied().unwrap_or(0);
    reply(&DebugPeersDto {
        id: row.id.clone(),
        name: clip(&ops::clean_name(&row.name), 200),
        state: row.state.clone(),
        truncated: (total - peers.len()) as u32,
        peers,
        connected: probe.connections_open.saturating_sub(probe.half_open),
        half_open: probe.half_open,
        max_connections: probe.max_connections,
        incoming_accepted: c("incoming_accepted"),
        incoming_refused: c("incoming_refused"),
    })
}

async fn debug_perf<B: Backend>(ctx: &Arc<Ctx<B>>) -> Result<Reply, ApiError> {
    let (rows, totals) = rows(ctx).await?;
    let limits = ctx.backend.limits().await?;
    let probe = ctx.backend.probe().await?;
    let process = tokio::task::spawn_blocking(crate::debug::process_stats).await.unwrap_or_default();
    let rank = |r: &TransferDto| match r.state.as_str() {
        "downloading" | "metadata" => 0,
        "checking" => 1,
        "queued" => 2,
        _ => 3,
    };
    let mut active: Vec<&TransferDto> = rows.iter().filter(|r| rank(r) < 3).collect();
    active.sort_by_key(|r| (rank(r), r.queue_pos));
    let torrents = active
        .iter()
        .take(25)
        .map(|r| DebugPerfTorrent {
            id: r.id.clone(),
            name: clip(&ops::clean_name(&r.name), 120),
            state: r.state.clone(),
            down_rate: r.down_rate,
            up_rate: r.up_rate,
            peers: r.peers,
            seeds: r.seeds,
            left_bytes: r.wanted_bytes.saturating_sub(r.done_bytes),
            queue_pos: r.queue_pos,
            waiting_note: r.waiting_note.clone(),
            stalled: r.state == "downloading" && r.down_rate < 2048,
        })
        .collect();
    reply(&DebugPerfDto {
        peers: rows.iter().map(|r| r.peers).sum(),
        torrent_count: rows.len() as u32,
        totals,
        limits,
        probe,
        process,
        torrents,
    })
}

/// `https://tracker.example` for any announce URL: scheme, host and port, nothing else (no path, no query,
/// so no passkey in any spelling).
pub(crate) fn host_only(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once("://") else { return "<tracker>".into() };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    format!("{scheme}://{host}")
}

#[cfg(test)]
mod host_only_tests {
    use super::host_only;

    #[test]
    fn announce_urls_keep_only_scheme_and_host() {
        assert_eq!(host_only("https://tracker.norbits.net/announce.php?passkey=abcdef0123456789abcdef0123456789"), "https://tracker.norbits.net");
        assert_eq!(host_only("http://u:p@t.org:8080/0123456789abcdef0123456789abcdef/announce"), "http://t.org:8080");
        assert_eq!(host_only("udp://t.org:6969"), "udp://t.org:6969");
        assert_eq!(host_only("garbage"), "<tracker>");
    }
}
