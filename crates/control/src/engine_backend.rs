//! The adapter from `trs_engine::EngineHandle` to the protocol. Mapping only: every rule
//! (permissions, ids, filters, redaction, confirmation) lives above [`crate::Backend`].
//! New engine fields are added to the `*_dto` functions below.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::broadcast;
use trs_engine::{
    AddOptions, AddSource as EngAddSource, AtGoal, PendingChange, PendingStatus, ChoiceGroupKind, ChoicePreview, ChoiceReason, ChoiceSelection, DownloadOptions,
    EngineError, EngineEvent, EngineHandle, FilePriority, StreamContainer, LibraryState, MoveKind, Priority, RowKind, SeedGoal, Tier, TorrentDetail,
    TorrentRow, TorrentState,
};

use crate::backend::{AddArgs, AddSource, Backend};
use crate::perm::PermissionSource;
use crate::proto::{ApiError, ErrorCode, FilePriorityParam, PriorityParam, SetLimitsParams};
use crate::redact::scrub_urls;
use crate::server::{ServerHandle, ServerOptions, serve_backend};
use crate::state::{StateAccess, StoreRef};
use crate::types::*;

/// [`Backend`] over the in-process engine.
pub struct EngineBackend {
    engine: EngineHandle,
    events: broadcast::Sender<EventDto>,
    #[cfg(feature = "sync")]
    sync: crate::sync::SyncSlot,
}

impl EngineBackend {
    /// Wrap a handle. Starts a small task that maps engine events to protocol events; it needs
    /// the tokio runtime that is current (or use [`start_for_app`], which takes the runtime).
    pub fn new(engine: EngineHandle) -> Self {
        Self::new_on(&tokio::runtime::Handle::current(), engine)
    }

    /// [`Self::new`] with the mapping task on `rt`.
    fn new_on(rt: &tokio::runtime::Handle, engine: EngineHandle) -> Self {
        let (tx, _) = broadcast::channel(512);
        let mut rx = engine.subscribe();
        let tx2 = tx.clone();
        rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(e) => {
                        if let Some(d) = event_dto(&e) {
                            let _ = tx2.send(d);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let _ = tx2.send(EventDto { kind: "lagged".into(), data: json!({ "missed": n }), ..Default::default() });
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Self {
            engine,
            events: tx,
            #[cfg(feature = "sync")]
            sync: crate::sync::SyncSlot::new(),
        }
    }

    /// Serve the folder sync service found in `slot` (set it once the service runs).
    #[cfg(feature = "sync")]
    pub fn with_sync(mut self, slot: crate::sync::SyncSlot) -> Self {
        self.sync = slot;
        self
    }
}

fn state_dto(s: TorrentState) -> &'static str {
    s.as_str()
}

fn pending_dto(p: &PendingStatus) -> PendingDto {
    PendingDto { starts_at_ms: p.deadline_ms, starts_in_secs: p.remaining_ms.div_ceil(1000), paused: p.paused }
}

fn tier_dto(t: Tier) -> &'static str {
    match t {
        Tier::Cold => "cold",
        Tier::Warm => "warm",
        Tier::Hot => "hot",
    }
}

fn priority_dto(p: Priority) -> &'static str {
    match p {
        Priority::High => "high",
        Priority::Normal => "normal",
        Priority::Low => "low",
    }
}

fn file_priority_dto(p: FilePriority) -> &'static str {
    match p {
        FilePriority::Skip => "skip",
        FilePriority::Low => "low",
        FilePriority::Normal => "normal",
        FilePriority::High => "high",
    }
}

fn goal_dto(g: &SeedGoal, private: bool) -> SeedGoalDto {
    SeedGoalDto {
        ratio: g.ratio,
        seed_secs: g.seed_secs,
        min_seed_secs: g.min_seed_secs,
        any: g.any,
        at_goal: match g.at_goal {
            AtGoal::Stop => "stop",
            AtGoal::Remove => "remove",
            AtGoal::RemoveAndDelete => "remove_and_delete",
        }
        .into(),
        stop_on_finish: g.stop_on_finish,
        idle_mb: g.idle_mb,
        idle_hours: g.idle_hours,
        text: g.describe_for(private),
    }
}

fn goal_from_dto(g: &SeedGoalDto) -> SeedGoal {
    SeedGoal {
        ratio: g.ratio,
        seed_secs: g.seed_secs,
        min_seed_secs: g.min_seed_secs,
        any: g.any,
        at_goal: match g.at_goal.as_str() {
            "remove" => AtGoal::Remove,
            "remove_and_delete" => AtGoal::RemoveAndDelete,
            _ => AtGoal::Stop,
        },
        stop_on_finish: g.stop_on_finish,
        idle_mb: g.idle_mb,
        idle_hours: g.idle_hours,
    }
}

fn row_dto(r: &TorrentRow) -> TransferDto {
    // A finished torrent that downloads again counts from where it was (the session), not from zero.
    let progress = if r.session_size > 0 {
        (r.session_done as f64 / r.session_size as f64).clamp(0.0, 1.0)
    } else if r.wanted_bytes == 0 {
        0.0
    } else {
        (r.done_bytes as f64 / r.wanted_bytes as f64).clamp(0.0, 1.0)
    };
    TransferDto {
        id: id_hex(&r.id),
        kind: match r.kind {
            RowKind::Torrent => "torrent",
            RowKind::Direct => "direct",
        }
        .into(),
        name: r.name.clone(),
        title: r.title.clone(),
        state: state_dto(r.state).into(),
        progress,
        total_bytes: r.total_bytes,
        wanted_bytes: r.wanted_bytes,
        done_bytes: r.done_bytes,
        down_rate: r.down_rate,
        up_rate: r.up_rate,
        peers: r.peers,
        seeds: r.seeds,
        peers_known: r.known_peers.max(r.peers),
        eta_secs: r.eta_secs,
        ratio: r.ratio,
        tracker_host: r.tracker_host.clone(),
        tracker_profile: r.tracker_profile.clone(),
        tier: tier_dto(r.tier).into(),
        private: r.private,
        added_at: r.added_at,
        finished_at: r.finished_at,
        // A tracker problem reads as the engine's plain sentence; a disk error stays as it is.
        error: if r.disk_error.is_none() && r.error.is_some() { r.tracker_detail.clone().or_else(|| r.error.clone()) } else { r.error.clone() },
        save_path: r.save_path.display().to_string(),
        priority: priority_dto(r.priority).into(),
        queue_pos: r.queue_pos,
        uploaded_total: r.uploaded_total,
        downloaded_total: r.downloaded_total,
        category: r.category.map(|c| c.as_str().to_owned()),
        seed_goal: goal_dto(&r.seed_goal, r.private),
        seed_goal_user: r.seed_goal_user,
        idle: r.idle_info(),
        idle_stopped: r.idle_reason(),
        seeded_secs: r.seeded_secs,
        rule: r.rule.clone(),
        url: r.url.clone(),
        up_limit: r.up_limit,
        organized: r.organized,
        last_activity: r.last_activity,
        error_since: r.error_since,
        next_retry_at: r.next_retry_ms.map(|m| m / 1000),
        waiting_note: r.waiting_note().map(|n| n.text),
        moving: r.moving,
        move_note: r.move_note.clone(),
        pending: r.pending.as_ref().map(pending_dto),
        human: Human::default(),
    }
}

fn detail_dto(d: &TorrentDetail) -> DetailDto {
    DetailDto {
        transfer: row_dto(&d.row),
        files: d
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| FileDto {
                index: i as u32,
                path: f.path.clone(),
                length: f.length,
                done: f.done,
                priority: file_priority_dto(f.priority).into(),
                padding: f.padding,
            })
            .collect(),
        trackers: d
            .trackers
            .iter()
            .map(|t| TrackerDto {
                url: t.url.clone(),
                tier: t.tier,
                status: t.status.clone(),
                seeds: t.seeds,
                leechers: t.leechers,
                last_announce_at: t.last_announce_ms.map(|m| m / 1000),
                next_announce_at: t.next_announce_ms.map(|m| m / 1000),
                state: t.state.as_str().to_owned(),
                failures: t.failures,
                last_success_at: t.last_success_ms.map(|m| m / 1000),
                retry_at: t.retry_at_ms.map(|m| m / 1000),
                manual_ok_at: t.manual_ok_ms.map(|m| m / 1000),
                tripped: t.tripped.map(str::to_owned),
            })
            .collect(),
        peers: d
            .peers
            .iter()
            .map(|p| PeerDto {
                addr: p.addr.to_string(),
                client: p.client.clone(),
                outgoing: p.outgoing,
                down_rate: p.down_rate,
                up_rate: p.up_rate,
                progress: p.progress,
                choking_us: p.choking_us,
                choked_by_us: p.choked_by_us,
                interested: p.interested,
                utp: p.utp,
                requests_in_flight: p.requests_in_flight,
                connected_secs: p.connected_secs,
                snubbed: p.snubbed,
                upload_only: p.upload_only,
                has_pex: p.has_pex,
            })
            .collect(),
        piece_count: d.piece_count,
        piece_length: d.piece_length,
        comment: d.comment.clone(),
        ..Default::default()
    }
}

fn reason_dto(r: &ChoiceReason) -> ChoiceReasonDto {
    use crate::human::bytes as b;
    let (kind, text) = match r {
        ChoiceReason::SeveralItems { items } => ("several_items", format!("{items} items to choose from")),
        ChoiceReason::InLibrary { items } => ("in_library", format!("{items} items are already in the library")),
        ChoiceReason::PlatformVariants => ("platform_variants", "builds for several systems; this Mac's build is preselected".to_owned()),
        ChoiceReason::VeryLarge { bytes, limit } => ("very_large", format!("{} is over the {} limit", b(*bytes), b(*limit))),
        ChoiceReason::NotEnoughSpace { needed, free } => ("not_enough_space", format!("needs {}, only {} free", b(*needed), b(*free))),
        ChoiceReason::Unsure { confidence } => ("unsure", format!("the category is only {:.0}% certain", f64::from(*confidence) * 100.0)),
        ChoiceReason::FolderAsk => ("folder_ask", "the watch folder asks first".to_owned()),
        ChoiceReason::Always => ("always", "Settings ask before adding every torrent".to_owned()),
    };
    ChoiceReasonDto { kind: kind.into(), text }
}

fn choice_dto(c: &ChoicePreview) -> ChoiceDto {
    let group_kind = |k: ChoiceGroupKind| match k {
        ChoiceGroupKind::Season(_) => "season",
        ChoiceGroupKind::Specials => "specials",
        ChoiceGroupKind::Collection => "collection",
        ChoiceGroupKind::Album => "album",
        ChoiceGroupKind::Platform => "platform",
        ChoiceGroupKind::Extras => "extras",
        ChoiceGroupKind::Other => "other",
    };
    let items: usize = c.groups.iter().map(|g| g.items.len()).sum();
    ChoiceDto {
        awaiting: c.awaiting,
        needs_choice: c.needs_choice,
        reasons: c.reasons.iter().map(reason_dto).collect(),
        structure: c.structure.to_owned(),
        groups: c
            .groups
            .iter()
            .map(|g| ChoiceGroupDto {
                label: g.label.clone(),
                kind: group_kind(g.kind).into(),
                items: g
                    .items
                    .iter()
                    .map(|i| ChoiceItemDto {
                        label: i.label.clone(),
                        bytes: i.bytes,
                        files: i.files.clone(),
                        selected: i.selected,
                        library: match i.library {
                            LibraryState::Missing => "missing",
                            LibraryState::InLibrary { .. } => "in_library",
                            LibraryState::OtherQuality { .. } => "other_quality",
                        }
                        .into(),
                        quality: i.quality.clone(),
                        this_mac: i.this_mac,
                        platform: i.platform.map(|(o, c)| format!("{o} {c}")),
                    })
                    .collect(),
            })
            .collect(),
        files: c.files.clone(),
        selected_bytes: c.selected_bytes,
        selected_files: c.selected_files,
        total_bytes: c.total_bytes,
        free_bytes: c.free_bytes,
        destination: c.destination.as_ref().map(|p| p.display().to_string()),
        category: c.category.map(|c| c.as_str().to_owned()),
        confidence: c.confidence,
        rule: c.rule_name.clone(),
        then: c.goal.then_phrase_for(c.private),
        rule_summary: c.summary().text(),
        summary: format!(
            "{}: {items} items, {} selected of {}{}",
            if c.structure.is_empty() { "files" } else { c.structure },
            crate::human::bytes(c.selected_bytes),
            crate::human::bytes(c.total_bytes),
            c.free_bytes.map(|f| format!(", {} free", crate::human::bytes(f))).unwrap_or_default()
        ),
    }
}

fn move_item_dto(i: &trs_engine::MoveItem) -> MoveItemDto {
    MoveItemDto {
        id: id_hex(&i.id),
        name: i.name.clone(),
        bytes: i.bytes,
        from: i.from.display().to_string(),
        to: i.to.display().to_string(),
        cross_volume: i.cross_volume,
        kind: match i.kind {
            MoveKind::Finished => "finished",
            MoveKind::Incomplete => "incomplete",
            MoveKind::Retarget => "retarget",
        }
        .into(),
    }
}

fn err(e: EngineError) -> ApiError {
    let msg = scrub_urls(&e.to_string());
    match e {
        EngineError::Invalid(_) | EngineError::NoMetadata => ApiError::invalid(msg),
        EngineError::Duplicate(id) => ApiError::new(ErrorCode::Duplicate, msg).with_data(json!({ "id": id_hex(&id) })),
        EngineError::TrackersMerged { id, added, .. } => {
            ApiError::new(ErrorCode::Duplicate, msg).with_data(json!({ "id": id_hex(&id), "trackers_added": added }))
        }
        EngineError::NotFound => ApiError::not_found(msg),
        EngineError::ShuttingDown => ApiError::new(ErrorCode::ShuttingDown, msg),
        _ => ApiError::new(ErrorCode::Engine, msg),
    }
}

fn prio(p: PriorityParam) -> Priority {
    match p {
        PriorityParam::High => Priority::High,
        PriorityParam::Normal => Priority::Normal,
        PriorityParam::Low => Priority::Low,
    }
}

fn file_prio(p: FilePriorityParam) -> FilePriority {
    match p {
        FilePriorityParam::Skip => FilePriority::Skip,
        FilePriorityParam::Low => FilePriority::Low,
        FilePriorityParam::Normal => FilePriority::Normal,
        FilePriorityParam::High => FilePriority::High,
    }
}

fn category(c: &Option<String>) -> Option<trs_classify::Category> {
    c.as_deref().and_then(trs_classify::Category::parse)
}

/// Map an engine event to a protocol event. Events that carry nothing an agent needs, or that
/// would expose peer addresses, map to `None`.
pub fn event_dto(e: &EngineEvent) -> Option<EventDto> {
    let ev = |kind: &str, id: Option<&[u8; 20]>, name: Option<&str>, data: Value| EventDto {
        kind: kind.into(),
        id: id.map(id_hex),
        name: name.map(str::to_owned),
        data,
        ..Default::default()
    };
    Some(match e {
        EngineEvent::Added { id, name } => ev("added", Some(id), Some(name), Value::Null),
        EngineEvent::MetadataReceived { id, name } => ev("metadata_received", Some(id), Some(name), Value::Null),
        EngineEvent::Finished { id } => ev("finished", Some(id), None, Value::Null),
        EngineEvent::StateChanged { id, state } => ev("state_changed", Some(id), None, json!({ "state": state_dto(*state) })),
        EngineEvent::Error { id, message } => ev("error", Some(id), None, json!({ "message": message })),
        EngineEvent::SeedingGoalReached { id } => ev("seeding_goal_reached", Some(id), None, Value::Null),
        EngineEvent::Removed { id } => ev("removed", Some(id), None, Value::Null),
        EngineEvent::AutoAdded { id, name, category, tracker_profile, .. } => ev(
            "auto_added",
            Some(id),
            Some(name),
            json!({ "category": category.as_str(), "tracker_profile": tracker_profile }),
        ),
        EngineEvent::Duplicate { id, name, trackers_added } => ev("duplicate", Some(id), Some(name), json!({ "trackers_added": trackers_added })),
        EngineEvent::WatchError { message, .. } => ev("watch_error", None, None, json!({ "message": message })),
        EngineEvent::ConfigReloaded { file, warnings, restart_required } => ev(
            "config_reloaded",
            None,
            None,
            json!({ "file": file, "warnings": warnings, "restart_required": restart_required }),
        ),
        EngineEvent::ConfigRejected { file, error } => ev("config_rejected", None, None, json!({ "file": file, "error": error })),
        EngineEvent::DirectRestarted { id, reason } => ev("direct_restarted", Some(id), None, json!({ "reason": reason })),
        EngineEvent::ChoiceNeeded { id, name, reasons, .. } => ev(
            "choice_needed",
            Some(id),
            Some(name),
            json!({ "reasons": reasons.iter().map(|r| reason_dto(r).text).collect::<Vec<_>>() }),
        ),
        EngineEvent::Pending { id, change } => ev(
            "pending",
            Some(id),
            None,
            json!({ "change": match change {
                PendingChange::Added => "added",
                PendingChange::Paused => "paused",
                PendingChange::Resumed => "resumed",
                PendingChange::Started => "started",
                PendingChange::Chosen => "chosen",
            } }),
        ),
        EngineEvent::ChoiceResolved { id, confirmed } => ev("choice_resolved", Some(id), None, json!({ "confirmed": confirmed })),
        EngineEvent::LocationsChanged { torrents, bytes } => ev("locations_changed", None, None, json!({ "torrents": torrents, "bytes": bytes })),
        EngineEvent::MoveStarted { id, from, to, bytes, cross_volume } => ev(
            "move_started",
            Some(id),
            None,
            json!({ "from": from.display().to_string(), "to": to.display().to_string(), "bytes": bytes, "cross_volume": cross_volume }),
        ),
        EngineEvent::MoveProgress { id, done, total } => ev("move_progress", Some(id), None, json!({ "done": done, "total": total })),
        EngineEvent::MoveFinished { id, to } => ev("move_finished", Some(id), None, json!({ "to": to.display().to_string() })),
        EngineEvent::MoveFailed { id, reason } => ev("move_failed", Some(id), None, json!({ "reason": scrub_urls(reason) })),
        // PeerBanned carries a peer address; AddRequest and PortMap are for the app's own UI.
        _ => return None,
    })
}

/// The engine's [`trs_engine::api::Reachability`] in protocol form, with its state and wording
/// (also what the app's Network settings show).
pub fn reachability_dto(r: &trs_engine::api::Reachability) -> ReachabilityDto {
    let pm = r.portmap.clone().unwrap_or_default();
    crate::ops::reachability(ReachabilityDto {
        port: r.port,
        mapping: r.mapping_enabled,
        method: pm.method.map(str::to_owned),
        external_port: pm.external_tcp_port,
        behind_cgnat: pm.behind_cgnat,
        router_unavailable: pm.unavailable,
        incoming: r.incoming,
        last_incoming_at: r.last_incoming_at,
        sync_external_port: pm.extra_external_tcp_port,
        sync_external_udp_port: pm.extra_external_udp_port,
        ..Default::default()
    })
}

impl Backend for EngineBackend {
    async fn snapshot(&self) -> Result<SnapshotDto, ApiError> {
        let s = self.engine.snapshot().await.map_err(err)?;
        Ok(SnapshotDto {
            transfers: s.rows.iter().map(row_dto).collect(),
            totals: TotalsDto {
                down_rate: s.down_rate,
                up_rate: s.up_rate,
                connections: s.connections,
                listen_port: s.listen_port,
                hot: s.counts.hot,
                warm: s.counts.warm,
                cold: s.counts.cold,
            },
        })
    }

    async fn limits(&self) -> Result<LimitsDto, ApiError> {
        let c = self.engine.config().await.map_err(err)?;
        Ok(LimitsDto {
            down_limit: c.effective_down(),
            up_limit: c.effective_up(),
            alt_active: c.alt_active(),
            alt_scheduled: c.alt_scheduled_on,
            alt_down_limit: c.alt_down_limit,
            alt_up_limit: c.alt_up_limit,
        })
    }

    async fn detail(&self, id: TransferId) -> Result<DetailDto, ApiError> {
        Ok(detail_dto(&self.engine.torrent_detail(id).await.map_err(err)?))
    }

    async fn add(&self, a: AddArgs) -> Result<AddedDto, ApiError> {
        let (added, kind) = match a.source {
            AddSource::Url(url) => {
                let opts = DownloadOptions {
                    dest_dir: a.save_path,
                    sha256: a.sha256,
                    paused: a.paused,
                    category: category(&a.category),
                    ..Default::default()
                };
                (self.engine.add_download(&url, opts).await.map_err(err)?, "direct")
            }
            src => {
                let opts = AddOptions {
                    save_path: a.save_path,
                    paused: a.paused,
                    category: category(&a.category),
                    priority: a.priority.map(prio),
                    peers: a.peers,
                    queue_top: a.queue_top,
                    rule: a.rule,
                    start_now: a.start_now,
                    ..Default::default()
                };
                let src = match src {
                    AddSource::Magnet(m) => EngAddSource::Magnet(m),
                    AddSource::TorrentFile(b) => EngAddSource::TorrentFile(b),
                    AddSource::Url(_) => return Err(ApiError::internal("unreachable source")),
                };
                (self.engine.add(src, opts).await.map_err(err)?, "torrent")
            }
        };
        Ok(AddedDto {
            id: id_hex(&added.id),
            name: added.name,
            kind: kind.into(),
            needs_choice: added.needs_choice,
            choice: added.choice.as_deref().map(choice_dto),
            pending: added.pending.as_ref().map(pending_dto),
        })
    }

    async fn pause(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.engine.pause(&ids);
        Ok(())
    }

    async fn resume(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.engine.resume(&ids);
        Ok(())
    }

    async fn pause_all(&self) -> Result<(), ApiError> {
        self.engine.pause_all();
        Ok(())
    }

    async fn resume_all(&self) -> Result<(), ApiError> {
        self.engine.resume_all();
        Ok(())
    }

    async fn move_to_top(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.engine.move_to_top(&ids);
        Ok(())
    }

    async fn set_priority(&self, ids: Vec<TransferId>, p: PriorityParam) -> Result<(), ApiError> {
        self.engine.set_priority(&ids, prio(p));
        Ok(())
    }

    async fn set_file_priorities(&self, id: TransferId, p: Vec<FilePriorityParam>) -> Result<(), ApiError> {
        self.engine.set_file_priorities(id, p.into_iter().map(file_prio).collect()).await.map_err(err)
    }

    async fn set_category(&self, ids: Vec<TransferId>, c: Option<String>) -> Result<(), ApiError> {
        self.engine.set_category(&ids, category(&c));
        Ok(())
    }

    async fn clear_seed_goal(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.engine.clear_seed_goal(&ids);
        Ok(())
    }
    async fn set_seed_goal(&self, ids: Vec<TransferId>, g: SeedGoalDto) -> Result<(), ApiError> {
        self.engine.set_seed_goal(&ids, goal_from_dto(&g));
        Ok(())
    }

    async fn set_limits(&self, p: SetLimitsParams) -> Result<LimitsDto, ApiError> {
        let nz = |v: u64| (v != 0).then_some(v);
        self.engine.update_config(move |c| {
            if let Some(v) = p.down_limit {
                c.down_limit = nz(v);
            }
            if let Some(v) = p.up_limit {
                c.up_limit = nz(v);
            }
            if let Some(v) = p.alt_down_limit {
                c.alt_down_limit = nz(v);
            }
            if let Some(v) = p.alt_up_limit {
                c.alt_up_limit = nz(v);
            }
            if let Some(v) = p.alt_speed {
                c.alt_speed = v;
            }
        });
        // `config()` queues behind the update, so this reads the new values.
        self.limits().await
    }

    async fn reannounce(&self, ids: Vec<TransferId>, force: bool, wait: bool) -> Result<Vec<ReannounceDto>, ApiError> {
        // The engine answers per tracker: `sent` now, or the earliest time the tracker may be asked
        // and why (Seedwell's own backoff never holds a user's ask back; the tracker's minimum
        // interval does unless `force`, which is rate limited).
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let results = self.engine.reannounce_with(&ids, force).await.map_err(err)?;
        let mut out = Vec::new();
        for id in ids {
            let mut trackers: Vec<ReannounceTracker> = results
                .iter()
                .filter(|r| r.id == id)
                .map(|r| {
                    // A second ask while the first still waits for its answer is not sent either: the
                    // tracker's minimum interval (which the first ask started) holds it.
                    let held = if force { None } else { r.min_interval_until_ms.filter(|_| r.sent) };
                    let out_now = r.sent && held.is_none();
                    ReannounceTracker {
                        url: r.url_redacted.clone(),
                        status: if out_now {
                            if force { "sent (forced past the tracker's minimum interval)".to_owned() } else { "sent".to_owned() }
                        } else if let Some(t) = r.force_limited_until_ms {
                            format!("deferred: forced announces are limited to one per tracker per 5 minutes; allowed at {}", t / 1000)
                        } else if let Some(n) = r.waiting_after_failures {
                            format!("waiting: Seedwell is backing off after {n} failed {}", if n == 1 { "try" } else { "tries" })
                        } else {
                            "deferred: the tracker asked for a minimum time between announces (--force overrides)".to_owned()
                        },
                        not_before: if out_now { None } else { held.or(r.not_before_ms).map(|m| m / 1000) },
                        answer: None,
                    }
                })
                .collect();
            if wait {
                // Up to 30 s for the tracker's answer to what was just sent.
                let urls: Vec<String> = results.iter().filter(|r| r.id == id && r.sent).map(|r| r.url_redacted.clone()).collect();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                let mut answers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
                while answers.len() < urls.len() && std::time::Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    let Ok(d) = self.engine.torrent_detail(id).await else { break };
                    for t in &d.trackers {
                        if !urls.contains(&t.url) || answers.contains_key(&t.url) {
                            continue;
                        }
                        if t.last_success_ms.is_some_and(|m| m >= now_ms) {
                            let swarm = match (t.seeds, t.leechers) {
                                (Some(s), Some(l)) => format!(": {s} seeders, {l} leechers"),
                                _ => String::new(),
                            };
                            answers.insert(t.url.clone(), format!("ok{swarm}"));
                        } else if t.last_announce_ms.is_some_and(|m| m >= now_ms) && t.problem.is_some() {
                            answers.insert(t.url.clone(), t.problem.clone().unwrap_or_default());
                        }
                    }
                }
                for tr in &mut trackers {
                    if let Some(a) = answers.get(&tr.url) {
                        tr.answer = Some(a.clone());
                    } else if urls.contains(&tr.url) {
                        tr.answer = Some("no answer yet (still waiting)".to_owned());
                    }
                }
            }
            let d = self.engine.torrent_detail(id).await.map_err(err)?;
            let note = if trackers.is_empty() {
                "no trackers to ask (DHT and peer exchange still run for public torrents)".to_owned()
            } else if trackers.iter().all(|t| t.status.starts_with("sent")) {
                "asked every tracker now".to_owned()
            } else {
                "some trackers could not be asked yet; see each tracker's status and not_before".to_owned()
            };
            out.push(ReannounceDto { id: id_hex(&id), name: d.row.name.clone(), trackers, note });
        }
        Ok(out)
    }

    async fn recheck(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.engine.recheck(&ids);
        Ok(())
    }

    async fn remove(&self, ids: Vec<TransferId>, delete_files: bool) -> Result<(), ApiError> {
        self.engine.remove(&ids, delete_files).await.map_err(err)
    }

    async fn choice(&self, id: TransferId) -> Result<ChoiceDto, ApiError> {
        Ok(choice_dto(&self.engine.choice(id).await.map_err(err)?))
    }

    async fn choose_files(&self, id: TransferId, files: Option<Vec<FilePriorityParam>>) -> Result<(), ApiError> {
        let sel = match files {
            None => ChoiceSelection::Default,
            Some(v) => ChoiceSelection::Files(v.into_iter().map(file_prio).collect()),
        };
        self.engine.confirm_choice(id, sel).await.map_err(err)
    }

    async fn cancel_choice(&self, id: TransferId) -> Result<(), ApiError> {
        self.engine.cancel_choice(id).await.map_err(err)
    }

    async fn cancel_held(&self, id: TransferId, pending: bool, keep_file: bool) -> Result<Option<String>, ApiError> {
        // The engine forgets where the file was the moment the torrent goes, so take the path first.
        let file = if keep_file { None } else { self.engine.held_source_for_cancel(id) };
        if pending {
            self.engine.remove(&[id], false).await.map_err(err)?;
        } else {
            self.engine.cancel_choice(id).await.map_err(err)?;
        }
        Ok(file.filter(|p| self.engine.trash_file(p)).and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())))
    }

    async fn start_now(&self, id: TransferId) -> Result<(), ApiError> {
        self.engine.start_now(id).await.map_err(err)
    }

    async fn sim_allow(&self, ids: Vec<TransferId>, allow: bool) -> Result<(), ApiError> {
        ids.into_iter().for_each(|id| self.engine.set_simulated(id, allow));
        Ok(())
    }

    async fn reachability(&self) -> Option<ReachabilityDto> {
        Some(reachability_dto(&self.engine.reachability()))
    }

    async fn probe(&self) -> Result<ProbeDto, ApiError> {
        let snap = self.engine.snapshot().await.map_err(err)?;
        let p = self.engine.probe();
        let d = self.engine.diagnostics();
        let pm = p.portmap.clone().unwrap_or_default();
        let counters: std::collections::BTreeMap<String, u64> = [
            ("dht_lookups", d.dht_lookups),
            ("dht_announces", d.dht_announces),
            ("pex_sent", d.pex_sent),
            ("pex_received", d.pex_received),
            ("hash_failures", d.hash_failures),
            ("peers_banned", d.peers_banned),
            ("incoming_accepted", d.incoming_accepted),
            ("incoming_refused", d.incoming_refused),
            ("rechecks", d.rechecks),
            ("utp_accepted", d.utp_accepted),
            ("utp_dialed", d.utp_dialed),
            ("mse_accepted", d.mse_accepted),
            ("mse_dialed", d.mse_dialed),
            ("mse_fallbacks", d.mse_fallbacks),
            ("verify_peak", d.verify_peak),
            ("boot_waiting", d.boot_waiting),
            ("wakeups", d.wakeups),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        Ok(ProbeDto {
            online: snap.online,
            offline_since: snap.offline_since_ms.map(|m| m / 1000),
            listen_port: snap.listen_port,
            port_fallback: snap.port_fallback.as_ref().map(|f| format!("wanted {}, using {}", f.wanted, f.using)),
            down_limited: p.down_limited,
            up_limited: p.up_limited,
            tracker_caps: p.tracker_groups as u32,
            verify_slots: p.verify_slots as u32,
            verify_free: p.verify_free as u32,
            max_connections: p.max_connections as u32,
            max_half_open: p.max_half_open as u32,
            connections_open: p.connections_open as u32,
            half_open: p.half_open as u32,
            dht_running: p.dht_running,
            portmap_method: pm.method.map(str::to_owned),
            portmap_reported: p.portmap.is_some(),
            portmap_unavailable: pm.unavailable,
            portmap_behind_cgnat: pm.behind_cgnat,
            external_port: pm.external_tcp_port,
            counters,
        })
    }

    async fn location_moves(&self) -> Result<MoveProposalDto, ApiError> {
        let p = self.engine.location_moves().await.map_err(err)?;
        Ok(MoveProposalDto { items: p.items.iter().map(move_item_dto).collect(), total_bytes: p.total_bytes, cross_volume: p.cross_volume })
    }

    async fn move_to(
        &self,
        ids: Vec<TransferId>,
        path: Option<PathBuf>,
    ) -> Result<crate::backend::MoveOutcome, ApiError> {
        let rep = match path {
            Some(p) => self.engine.move_to(&ids, &p).await,
            None => self.engine.move_torrents(&ids).await,
        }
        .map_err(err)?;
        Ok((rep.queued, rep.refused))
    }

    async fn cancel_moves(&self, ids: Vec<TransferId>) -> Result<(), ApiError> {
        self.engine.cancel_moves(&ids);
        Ok(())
    }

    async fn stream_url(&self, id: TransferId, file: Option<u32>) -> Result<StreamInfoDto, ApiError> {
        let file = match file {
            Some(f) => f,
            None => {
                // The largest video or audio file; else simply the largest.
                let d = self.engine.torrent_detail(id).await.map_err(err)?;
                let media = |p: &str| StreamContainer::of_name(p.rsplit('/').next().unwrap_or(p)) != StreamContainer::Unknown;
                let best = |only_media: bool| {
                    d.files
                        .iter()
                        .enumerate()
                        .filter(|(_, f)| !f.padding && f.length > 0 && (!only_media || media(&f.path)))
                        .max_by_key(|(_, f)| f.length)
                        .map(|(i, _)| i as u32)
                };
                best(true).or_else(|| best(false)).ok_or_else(|| ApiError::invalid("the transfer has no file to play"))?
            }
        };
        let i = self.engine.stream_url(id, file).await.map_err(err)?;
        Ok(StreamInfoDto {
            url: i.url,
            token: i.token,
            id: id_hex(&id),
            file: i.file_index,
            file_name: i.file_name,
            file_len: i.file_len,
            content_type: i.content_type,
            container: i.container.as_str().to_owned(),
            is_mp4_family: i.is_mp4_family,
            complete: i.complete,
            selected: i.selected,
        })
    }

    async fn stop_stream(&self, token: &str) -> Result<bool, ApiError> {
        Ok(self.engine.stop_stream(token))
    }

    async fn trash_dir(&self) -> Option<PathBuf> {
        self.engine.config().await.ok().and_then(|c| c.trash_dir)
    }

    #[cfg(feature = "sync")]
    fn sync(&self) -> Option<trs_sync_net::SyncHandle> {
        self.sync.get()
    }

    fn subscribe(&self) -> broadcast::Receiver<EventDto> {
        self.events.subscribe()
    }
}

/// Serve the in-process engine on `path` (see [`serve_backend`]).
pub async fn serve(
    engine: EngineHandle,
    state: StateAccess,
    path: impl AsRef<std::path::Path>,
    perm: PermissionSource,
) -> io::Result<ServerHandle> {
    serve_backend(Arc::new(EngineBackend::new(engine)), state, path, perm, ServerOptions::default()).await
}

/// [`serve`] with explicit [`ServerOptions`].
pub async fn serve_with(
    engine: EngineHandle,
    state: StateAccess,
    path: impl AsRef<std::path::Path>,
    perm: PermissionSource,
    options: ServerOptions,
) -> io::Result<ServerHandle> {
    serve_backend(Arc::new(EngineBackend::new(engine)), state, path, perm, options).await
}

/// Everything the app passes to start the control server.
pub struct AppServerConfig {
    /// Socket path; `None` is `~/Library/Application Support/Seedwell/control.sock`.
    pub socket_path: Option<PathBuf>,
    /// Directory with `config.toml`, `rules.toml`, `trackers.toml`.
    pub state_dir: PathBuf,
    /// The open database (the engine's store adapter), for statistics.
    pub store: Option<Arc<dyn StoreRef>>,
    /// Where the permission level comes from (the app's Settings).
    pub permission: PermissionSource,
    /// Limits and names; `ServerOptions::default().with_config(&config)` is the product's.
    pub options: ServerOptions,
    /// Local UTC offset in seconds, for "today" in statistics.
    pub tz_offset: Option<Arc<dyn Fn() -> i32 + Send + Sync>>,
}

/// Start the control server for an in-process engine. Called once by `trs-ffi` after the engine
/// starts. `rt` is the engine's runtime; the server and its tasks live on it. Keep the returned
/// handle for the life of the app (dropping it closes the socket); `handle.activity()` is the
/// activity log for Settings -> Agents.
pub fn start_for_app(rt: &tokio::runtime::Handle, engine: EngineHandle, cfg: AppServerConfig) -> io::Result<ServerHandle> {
    outside_runtime()?;
    start_for_app_inner(rt, EngineBackend::new_on(rt, engine), cfg)
}

fn outside_runtime() -> io::Result<()> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(io::Error::other("start_for_app blocks: call it from outside the runtime, or use `serve(..).await`"));
    }
    Ok(())
}

/// [`start_for_app`] serving folder sync too: the `sync.*` methods use the service in `slot` (put it in
/// once [`crate::sync::start_sync`] has started it; until then they say sync is not running).
#[cfg(feature = "sync")]
pub fn start_for_app_with_sync(
    rt: &tokio::runtime::Handle,
    engine: EngineHandle,
    cfg: AppServerConfig,
    slot: crate::sync::SyncSlot,
) -> io::Result<ServerHandle> {
    outside_runtime()?;
    start_for_app_inner(rt, EngineBackend::new_on(rt, engine).with_sync(slot), cfg)
}

fn start_for_app_inner(rt: &tokio::runtime::Handle, backend: EngineBackend, cfg: AppServerConfig) -> io::Result<ServerHandle> {
    let path = cfg.socket_path.unwrap_or_else(crate::socket::default_socket_path);
    let mut state = StateAccess::new(cfg.state_dir, cfg.store);
    if let Some(tz) = cfg.tz_offset {
        state = state.with_tz_offset(move || tz());
    }
    // The app holds a power assertion while an agent's lease is live.
    let options = ServerOptions { keeps_awake: true, ..cfg.options };
    rt.block_on(async move { serve_backend(Arc::new(backend), state, path, cfg.permission, options).await })
}

impl StoreRef for trs_engine::StateStore {
    fn store(&self) -> &trs_state::Store {
        self.state()
    }
}
