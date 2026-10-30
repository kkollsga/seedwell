//! Adding torrents: `.torrent` files (parsed and planned off the actor) and magnet links, and the
//! second half of a magnet add once its metadata is in.

use std::net::SocketAddr;
use tokio::sync::oneshot;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use trs_classify::Category;
use trs_metainfo::{Magnet, Metainfo};
use trs_tracker::TrackerUrl;

use super::cold::*;
use super::{Entry, Session, SessionMsg};
use crate::api::*;
use crate::counters::Counters;
use crate::organize::{self, Plan, PlanCtx, Prepared};
use crate::persist::{Batch, StatDelta};
use crate::policy;
use crate::tracker_driver::{TrackerCmd, TrackerTorrent};
use crate::util::{hex, unix_secs};

/// Result of classifying and placing a magnet whose metadata just arrived.
pub(crate) struct MetaPlanned {
    pub id: TorrentId,
    /// The synthesized `.torrent` (announce-list + the verified info dictionary).
    pub file: Arc<[u8]>,
    pub name: String,
    pub total_length: u64,
    pub wanted_length: u64,
    pub piece_length: u32,
    pub private: bool,
    pub plan: Plan,
    pub peers: Vec<SocketAddr>,
    /// Smart add: hold the torrent for a choice now that its files are known.
    pub hold: bool,
}

/// Most announce URLs a duplicate add may merge into one torrent over its life.
pub(crate) const MAX_MERGED_TRACKERS: usize = 8;

impl Session {
    /// Commits `b` now, past the writer. A removal of one of its torrents that is not stored yet goes first (a
    /// torrent removed and added again at once keeps what the add wrote).
    pub(super) fn commit_now(&mut self, b: Batch) -> Result<(), EngineError> {
        let ids: Vec<TorrentId> = b
            .torrents
            .iter()
            .map(|r| r.id)
            .chain(b.torrent_files.iter().map(|(i, _)| *i))
            .chain(b.directs.iter().map(|d| d.id))
            .chain(b.placements.iter().map(|(i, _)| *i))
            .chain(b.resume.iter().map(|(i, _)| *i))
            .collect();
        self.before_commit_now(ids);
        self.sh.store.commit(b).map_err(|e| EngineError::Store(e.to_string()))
    }

    /// Where unfinished data of `c` lives while it is staged.
    pub(crate) fn staging_for(&self, c: &ColdTorrent) -> Option<PathBuf> {
        if !c.has(F_STAGED) {
            return None;
        }
        let save = self.save_path(c);
        // The root recorded when the torrent was added or last moved wins; records from before
        // per-category locations follow the configured folder.
        let root = self.stage_root(c).or_else(|| self.sh.cfg().incomplete_dir.clone());
        Some(match root {
            Some(inc) => inc.join(hex(&c.id)),
            None => save.join(".incomplete"),
        })
    }

    fn new_cold(&mut self, id: TorrentId, name: &str, opts: &AddOptions, save: PathBuf) -> ColdTorrent {
        let root = self.intern_root(save);
        let pos = if opts.queue_top { 0 } else { self.next_pos };
        self.next_pos = self.next_pos.max(pos) + 1;
        let mut c = ColdTorrent {
            id,
            name: name.into(),
            title: None,
            counters: Arc::new(Counters::default()),
            total_len: 0,
            wanted_len: 0,
            session_base: 0,
            seed_goal: SeedGoal::FOREVER,
            idle: None,
            file_prios: None,
            error: None,
            magnet: None,
            piece_len: 0,
            save_root: root,
            stage_root: NONE_U32,
            added_at: unix_secs() as u32,
            finished_at: 0,
            queue_pos: pos,
            tracker_profile: NONE_U32,
            rule: NONE_U32,
            up_limit_kib: 0,
            last_activity: unix_secs() as u32,
            error_since: 0,
            avg_up: 0,
            seeded_secs: 0,
            seed_since: 0,
            host: 0,
            want: if opts.paused { Want::Paused } else { Want::Active },
            priority: Priority::Normal.to_u8(),
            category: NONE_CAT,
            flags: 0,
            flags2: G_UNVERIFIED,
            flags3: 0,
        };
        // A save path chosen by the caller is the user's choice of location: automatic location
        // changes (settings, category) leave the torrent alone.
        c.set2(G_PINNED, opts.save_path.is_some());
        c
    }

    /// Copy what the plan decided into the Cold record.
    fn apply_plan(&mut self, id: TorrentId, plan: &Plan, opts_rule: Option<&str>) {
        let profile = plan.profile.profile.as_deref().and_then(|p| self.ids.intern(p));
        let rule = opts_rule.and_then(|r| self.ids.intern(r));
        let stage = plan.stage_root.clone().map(|p| self.intern_root(p));
        let Some(e) = self.table.get_mut(&id) else { return };
        let c = &mut e.cold;
        c.stage_root = stage.unwrap_or(NONE_U32);
        c.tracker_profile = profile.unwrap_or(NONE_U32);
        c.rule = rule.unwrap_or(NONE_U32);
        c.set_category(plan.category);
        c.title = Some(plan.title.as_str().into());
        c.set2(G_CAT_USER, plan.category_user);
        c.priority = plan.priority.to_u8();
        c.set2(G_PRIO_USER, plan.priority_user);
        c.seed_goal = plan.goal;
        c.set2(G_GOAL_USER, plan.goal_user);
        c.set_up_limit(plan.resolved.up_limit);
        c.file_prios = (!plan.file_prios.is_empty()).then(|| plan.file_prios.clone().into_boxed_slice());
    }

    /// The torrent is already here. A public one takes the new announce URLs (SPEC 7A.1); the
    /// answer says which: `TrackersMerged` when some were new, `Duplicate` otherwise.
    fn duplicate(&mut self, id: TorrentId, incoming_private: bool, offered: &[String]) -> EngineError {
        let name = self.table.get(&id).map(|e| e.cold.name.to_string()).unwrap_or_default();
        let added = self.merge_duplicate_trackers(id, incoming_private, offered);
        self.sh.emit(EngineEvent::Duplicate { id, name: name.clone(), trackers_added: added });
        if added == 0 {
            EngineError::Duplicate(id)
        } else {
            EngineError::TrackersMerged { id, name, added }
        }
    }

    /// Merge announce URLs of a duplicate add into a public torrent: deduplicated (exact URL, the
    /// http/https twin, or another URL of a tracker profile the torrent already has), each its own
    /// tier, at most [`MAX_MERGED_TRACKERS`] over the torrent's life, never when either side is
    /// private (a magnet cannot say, so the existing torrent must have its metadata and be public).
    /// Persists the new list and hands the URLs to the tracker driver, which announces to them
    /// through the normal host gate. Returns how many were added.
    fn merge_duplicate_trackers(&mut self, id: TorrentId, incoming_private: bool, offered: &[String]) -> usize {
        if incoming_private || offered.is_empty() {
            return 0;
        }
        let cfg = self.sh.cfg();
        let Some(e) = self.table.get(&id) else { return 0 };
        if e.cold.private() || !e.cold.has_metadata() {
            return 0;
        }
        let Ok(all) = self.sh.store.load_torrents() else { return 0 };
        let Some(st) = all.into_iter().find(|s| s.record.id == id) else { return 0 };
        if st.record.private {
            return 0;
        }
        let key = |u: &str| u.trim().to_ascii_lowercase();
        let mut tiers = st.record.announce.clone().unwrap_or_default();
        // What the torrent came with does not count against the cap.
        let mut original: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Ok(Some(bytes)) = self.sh.store.load_torrent_file(&id)
            && let Ok(mi) = Metainfo::from_bytes(&bytes) {
                original.extend(mi.announce_tiers.iter().flatten().map(|u| key(u)));
            }
        if let Some(m) = st.record.magnet.as_deref().and_then(|u| Magnet::parse(u).ok()) {
            original.extend(m.trackers.iter().map(|u| key(u)));
        }
        let mut have: std::collections::HashSet<String> = tiers.iter().flatten().map(|u| key(u)).collect();
        let mut parsed: Vec<TrackerUrl> = tiers.iter().flatten().filter_map(|u| TrackerUrl::parse(u).ok()).collect();
        let mut room = MAX_MERGED_TRACKERS.saturating_sub(have.iter().filter(|k| !original.contains(*k)).count());
        let pol = self.sh.policy();
        let profile_of = |u: &str| pol.as_deref().and_then(|p| policy::match_profile(&p.profiles, [u]).profile);
        let mut profiles: Vec<String> = tiers.iter().flatten().filter_map(|u| profile_of(u)).collect();
        let mut added: Vec<String> = Vec::new();
        for u in offered {
            let u = u.trim();
            if room == 0 {
                break;
            }
            let Ok(url) = TrackerUrl::parse(u) else { continue };
            if have.contains(&key(u)) || parsed.iter().any(|x| trs_tracker::set::is_scheme_twin(x, &url)) {
                continue;
            }
            if let Some(pr) = profile_of(u) {
                if profiles.contains(&pr) {
                    continue;
                }
                profiles.push(pr);
            }
            have.insert(key(u));
            parsed.push(url);
            tiers.push(vec![u.to_owned()]);
            added.push(u.to_owned());
            room -= 1;
        }
        if added.is_empty() {
            return 0;
        }
        let rec = {
            let Some(e) = self.table.get(&id) else { return 0 };
            self.record_of(&e.cold, Some(tiers.clone()))
        };
        if self.commit_now(Batch { torrents: vec![rec], ..Batch::default() }).is_err() {
            return 0;
        }
        if cfg.trackers_enabled {
            let registered = self.table.get(&id).is_some_and(|e| e.cold.has(F_TRACKER_REG));
            if registered {
                self.tracker_send(TrackerCmd::AddTrackers(id, added.clone()));
            } else if let Some(e) = self.table.get_mut(&id) {
                e.cold.set(F_TRACKER_REG, true);
                let t = TrackerTorrent {
                    id,
                    private: false,
                    added_complete: e.cold.has(F_ADDED_COMPLETE),
                    tiers,
                    counters: e.cold.counters.clone(),
                    size: e.cold.total_len,
                };
                self.tracker_send(TrackerCmd::Add(t));
                self.sync_tracker(id);
            }
        }
        added.len()
    }

    pub(super) fn add_prepared(&mut self, p: Prepared, opts: AddOptions, origin: AddOrigin) -> Result<Added, EngineError> {
        let id = p.id;
        if self.table.contains_key(&id) {
            let urls: Vec<String> = p.announce.iter().flatten().cloned().collect();
            return Err(self.duplicate(id, p.private, &urls));
        }
        let cfg = self.sh.cfg();
        let plan = &p.plan;
        let mut cold = self.new_cold(id, &p.name, &opts, plan.save_path.clone());
        cold.total_len = p.total_length;
        cold.wanted_len = p.wanted_length;
        cold.piece_len = p.piece_length;
        cold.set(F_META, true);
        cold.set(F_PRIVATE, p.private);
        cold.set(F_STAGED, cfg.use_staging || cfg.incomplete_dir.is_some());
        // Nothing needs a choice: hold it for the grace period instead of starting it (SPEC 7A.7).
        let grace = if p.hold { None } else { self.grace_for_torrent(&opts) };
        cold.set2(G_AWAITING, p.hold || grace.is_some());
        if grace.is_some() {
            cold.flags3 |= H3_PENDING;
        }
        cold.counters.set_left(p.total_length);
        if let Some(h) = p.announce.first().and_then(|t| t.first()).and_then(|u| TrackerUrl::parse(u).ok()) {
            cold.host = self.intern_host(h.host());
        }
        self.table.insert(id, Entry { cold, live: None });
        self.apply_plan(id, plan, opts.rule.as_deref());
        let rec = {
            let e = &self.table[&id];
            self.record_of(&e.cold, Some(p.announce.clone()))
        };
        let batch = Batch {
            torrent_files: vec![(id, p.bytes.clone())],
            torrents: vec![rec],
            ..Batch::default()
        };
        if let Err(e) = self.commit_now(batch) {
            self.table.remove(&id);
            return Err(e);
        }
        let name = p.name.clone();
        let private = p.private;
        if let Some(d) = grace {
            self.arm_pending(id, d);
        }
        self.after_add(id, &name, &opts, private);
        let mut choice = None;
        if grace.is_some() {
            self.sh.emit(EngineEvent::Pending { id, change: PendingChange::Added });
            self.sh.emit(EngineEvent::StateChanged { id, state: TorrentState::Pending });
        }
        if p.hold {
            // Held: nothing is announced, no peer is contacted, nothing is allocated until the
            // user chooses (the actor is never started, see `reconcile`).
            let mut c = plan.choice.clone().unwrap_or_default();
            c.awaiting = true;
            if c.reasons.is_empty() {
                c.reasons.push(ChoiceReason::Always);
            }
            c.needs_choice = true;
            self.sh.emit(EngineEvent::ChoiceNeeded { id, name: name.clone(), reasons: c.reasons.clone(), origin: origin.clone() });
            self.sh.emit(EngineEvent::StateChanged { id, state: TorrentState::AwaitingChoice });
            choice = Some(Box::new(c));
        } else if let AddOrigin::Watch(_) = &origin {
            self.sh.emit(EngineEvent::AutoAdded {
                id,
                name: name.clone(),
                title: plan.title.clone(),
                category: plan.category.unwrap_or(Category::Other),
                tracker_profile: plan.profile.profile.clone(),
                destination: plan.destination.clone(),
                origin,
            });
        }
        let pending = self.pending_status(&id);
        Ok(Added { id, name, needs_choice: p.hold, choice, pending })
    }

    pub(super) fn add_magnet(&mut self, uri: &str, opts: AddOptions) -> Result<Added, EngineError> {
        let m = Magnet::parse(uri).map_err(|e| EngineError::Invalid(e.to_string()))?;
        let id = m.info_hash.swarm_id().ok_or_else(|| EngineError::Invalid("no info hash".into()))?;
        if self.table.contains_key(&id) {
            return Err(self.duplicate(id, false, &m.trackers));
        }
        let cfg = self.sh.cfg();
        let name = m.display_name.clone().unwrap_or_else(|| hex(&id));
        let save = opts.save_path.clone().unwrap_or_else(|| cfg.default_save_dir.clone());
        let mut cold = self.new_cold(id, &name, &opts, save.clone());
        cold.magnet = Some(uri.into());
        // A magnet is held later, when its metadata is in; remember what the caller said.
        if opts.hold == Some(false) || !opts.file_priorities.is_empty() {
            cold.flags3 |= H3_NO_HOLD;
        } else if opts.hold == Some(true) {
            cold.flags3 |= H3_FORCE_HOLD;
        }
        cold.set(F_STAGED, cfg.use_staging || cfg.incomplete_dir.is_some());
        // Held for the grace period: no announce, no DHT, no peers, no metadata fetch until it ends.
        let grace = self.grace_for_magnet(&opts);
        if grace.is_some() {
            cold.set2(G_AWAITING, true);
            cold.flags3 |= H3_PENDING;
        }
        let tiers: Vec<Vec<String>> = m.trackers.iter().map(|t| vec![t.clone()]).collect();
        if let Some(h) = tiers.first().and_then(|t| t.first()).and_then(|u| TrackerUrl::parse(u).ok()) {
            cold.host = self.intern_host(h.host());
        }
        // Profile and rule are known from the magnet's trackers; category and placement wait for
        // the file list. Until then rules see the torrent as `Other`.
        let policy = self.sh.policy();
        let pm = policy
            .as_deref()
            .map(|p| policy::match_profile(&p.profiles, tiers.iter().flatten().map(String::as_str)))
            .unwrap_or_default();
        let profile = pm.profile.as_deref().and_then(|p| self.ids.intern(p));
        let rule = opts.rule.as_deref().and_then(|r| self.ids.intern(r));
        cold.tracker_profile = profile.unwrap_or(NONE_U32);
        cold.rule = rule.unwrap_or(NONE_U32);
        if let Some(c) = opts.category {
            cold.set_category(Some(c));
            cold.set2(G_CAT_USER, true);
        }
        if let Some(p) = opts.priority {
            cold.priority = p.to_u8();
            cold.set2(G_PRIO_USER, true);
        }
        if let Some(g) = opts.seed_goal {
            cold.seed_goal = g;
            cold.set2(G_GOAL_USER, true);
        }
        cold.file_prios = (!opts.file_priorities.is_empty()
            && !opts.file_priorities.iter().all(|p| *p == FilePriority::Normal))
        .then(|| opts.file_priorities.iter().map(|p| p.to_u8()).collect());
        let rec = cold.record(save, None, Some(tiers.clone()), &self.ids);
        self.commit_now(Batch { torrents: vec![rec], ..Batch::default() })?;
        let mut peers = opts.peers.clone();
        peers.extend(m.peers.iter().copied());
        let mut opts = opts;
        opts.peers = peers;
        self.table.insert(id, Entry { cold, live: None });
        if policy.is_some()
            && let Some(p) = &policy {
                self.apply_policy_to(id, p);
            }
        // Magnets announce right away (size 0 = metadata unknown).
        if cfg.trackers_enabled && !tiers.is_empty() {
            self.tracker_send(TrackerCmd::Add(TrackerTorrent {
                id,
                private: false,
                added_complete: false,
                tiers,
                counters: self.table[&id].cold.counters.clone(),
                size: 0,
            }));
            if let Some(e) = self.table.get_mut(&id) {
                e.cold.set(F_TRACKER_REG, true);
            }
        }
        if let Some(d) = grace {
            self.arm_pending(id, d);
        }
        self.after_add(id, &name, &opts, false);
        if grace.is_some() {
            self.sh.emit(EngineEvent::Pending { id, change: PendingChange::Added });
            self.sh.emit(EngineEvent::StateChanged { id, state: TorrentState::Pending });
        }
        let pending = self.pending_status(&id);
        Ok(Added { id, name, needs_choice: false, choice: None, pending })
    }

    fn after_add(&mut self, id: TorrentId, name: &str, opts: &AddOptions, private: bool) {
        self.sh.emit(EngineEvent::Added { id, name: name.to_owned() });
        let domain = self.table.get(&id).and_then(|e| self.stat_domain(&e.cold));
        self.push_stat(StatDelta { domain, private, at: unix_secs(), torrents_added: 1, ..StatDelta::default() });
        let mut peers = opts.peers.clone();
        peers.truncate(256);
        if !peers.is_empty() {
            // Hints are handed to the actor when it starts.
            self.hint_peers.insert(id, peers);
        }
        self.ensure_goal_job();
        self.replan();
        self.reconcile(id);
        let spread = Duration::from_secs(1 + crate::util::unix_ms() % 30);
        self.dht_schedule_first(id, spread);
    }

    // ---- magnet metadata -------------------------------------------------------------------

    /// Options that reproduce the user's choices for a torrent already in the table.
    pub(super) fn opts_of(&self, c: &ColdTorrent) -> AddOptions {
        AddOptions {
            save_path: c.has2(G_PINNED).then(|| self.save_path(c)),
            category: c.has2(G_CAT_USER).then(|| c.category()).flatten(),
            priority: c.has2(G_PRIO_USER).then(|| c.priority()),
            seed_goal: c.has2(G_GOAL_USER).then_some(c.seed_goal),
            rule: ColdTorrent::opt(c.rule).and_then(|i| self.ids.get(i)).map(str::to_owned),
            file_priorities: c.file_prios.as_deref().map(|p| p.iter().map(|x| FilePriority::from_u8(*x)).collect()).unwrap_or_default(),
            ..AddOptions::default()
        }
    }

    /// Metadata arrived: classify and place off the actor, then continue in
    /// [`Session::on_metadata_planned`].
    pub(super) fn plan_metadata(&mut self, id: TorrentId, mi: Metainfo, file: Arc<[u8]>, peers: Vec<SocketAddr>) {
        if !self.planning.insert(id) {
            return;
        }
        let Some(e) = self.table.get(&id) else { return };
        let opts = self.opts_of(&e.cold);
        let cfg = self.sh.cfg();
        let policy = self.sh.policy();
        let sh = self.sh.clone();
        let (no_hold, force_hold) = (e.cold.flags3 & H3_NO_HOLD != 0, e.cold.flags3 & H3_FORCE_HOLD != 0);
        let choice = force_hold || (!no_hold && cfg.smart_add.mode != crate::config::AskMode::Never);
        tokio::task::spawn_blocking(move || {
            let ctx = PlanCtx { cfg: &cfg, policy: policy.as_deref(), choice, folder_ask: false };
            // `save_path` here is the torrent's current folder when the user pinned it.
            let plan = organize::plan(&mi, &opts, &ctx);
            let hold = force_hold || (!no_hold && plan.choice.as_ref().is_some_and(|c| c.needs_choice));
            let wanted = organize::wanted_bytes(&mi, &plan.file_prios);
            let msg = MetaPlanned {
                id,
                file,
                name: mi.name.to_string(),
                total_length: mi.total_length,
                wanted_length: wanted,
                piece_length: mi.piece_length,
                private: mi.private,
                plan,
                peers,
                hold,
            };
            let _ = sh.session.blocking_send(SessionMsg::MetadataPlanned(Box::new(msg)));
        });
    }

    pub(super) fn on_metadata_planned(&mut self, m: MetaPlanned) {
        let id = m.id;
        self.planning.remove(&id);
        if self.shutting_down || !self.table.get(&id).is_some_and(|e| !e.cold.has_metadata()) {
            return;
        }
        let rule_forced = self.table.get(&id).and_then(|e| ColdTorrent::opt(e.cold.rule)).and_then(|i| self.ids.get(i)).map(str::to_owned);
        let save = m.plan.save_path.clone();
        let cfg = self.sh.cfg();
        {
            let Some(e) = self.table.get_mut(&id) else { return };
            let c = &mut e.cold;
            c.name = m.name.clone().into();
            c.total_len = m.total_length;
            c.wanted_len = m.wanted_length;
            c.piece_len = m.piece_length;
            c.set(F_META, true);
            c.set(F_PRIVATE, m.private);
            c.set(F_STAGED, cfg.use_staging || cfg.incomplete_dir.is_some());
            c.set2(G_UNVERIFIED, true);
            c.set2(G_AWAITING, m.hold);
            c.magnet = None;
            c.counters.set_left(m.total_length);
        }
        // The user's earlier choices were fed into the plan, so applying it keeps them.
        self.apply_plan(id, &m.plan, rule_forced.as_deref());
        // The save root follows the plan (the finished location may have changed while the metadata was fetched).
        let root = self.intern_root(save.clone());
        if let Some(e) = self.table.get_mut(&id) {
            e.cold.save_root = root;
        }
        let rec = {
            let Some(e) = self.table.get(&id) else { return };
            self.record_of(&e.cold, None)
        };
        let batch = Batch {
            torrent_files: vec![(id, m.file.clone())],
            torrents: vec![rec],
            ..Batch::default()
        };
        if let Err(err) = self.commit_now(batch) {
            self.on_fault(id, crate::peer::FaultKind::Other, format!("cannot store the metadata: {err}"));
            return;
        }
        self.tracker_send(TrackerCmd::SetSize(id, m.total_length));
        self.hint_peers.insert(id, m.peers);
        let name = m.name;
        self.sh.emit(EngineEvent::MetadataReceived { id, name: name.clone() });
        if m.hold {
            // Metadata is in: stop requesting anything. The tracker session closes (`reconcile`
            // -> `sync_tracker`), the metadata actor ends, and no piece is ever requested until
            // the user chooses.
            let mut reasons = m.plan.choice.as_ref().map(|c| c.reasons.clone()).unwrap_or_default();
            if reasons.is_empty() {
                reasons.push(ChoiceReason::Always);
            }
            self.stop_live(id, super::After::Nothing);
            self.sync_tracker(id);
            self.sh.emit(EngineEvent::ChoiceNeeded { id, name, reasons, origin: AddOrigin::User });
            self.sh.emit(EngineEvent::StateChanged { id, state: TorrentState::AwaitingChoice });
            return;
        }
        // The metadata-only actor ends; a normal one takes over (with a first recheck).
        self.stop_live(id, super::After::Respawn { force_recheck: false });
    }
}

// ---- smart add: the chooser (SPEC 7A.6) ---------------------------------------------------------

impl Session {
    /// What the chooser computation needs; the work itself runs off the actor.
    pub(super) fn choice_job(&self, id: TorrentId) -> Result<super::ChoiceJob, EngineError> {
        let e = self.table.get(&id).ok_or(EngineError::NotFound)?;
        let c = &e.cold;
        if !c.has_metadata() {
            return Err(EngineError::NoMetadata);
        }
        Ok(super::ChoiceJob {
            opts: self.opts_of(c),
            awaiting: c.awaiting(),
            prios: c.file_prios.as_deref().map(<[u8]>::to_vec).unwrap_or_default(),
            save_path: self.save_path(c),
            stage_root: self.stage_root(c),
            placed: c.placed() != crate::persist::Placed::No,
            category: c.category(),
            rule: self.sh.policy().and_then(|p| self.rule_in_effect(c, &p)),
            goal: c.seed_goal,
        })
    }

    /// The user answered the chooser: set the priorities; a held torrent starts.
    pub(super) fn confirm_choice(&mut self, id: TorrentId, prios: Vec<FilePriority>) -> Result<(), EngineError> {
        let e = self.table.get(&id).ok_or(EngineError::NotFound)?;
        if !e.cold.has_metadata() {
            return Err(EngineError::NoMetadata);
        }
        let was_held = e.cold.awaiting();
        if !prios.iter().any(|p| *p != FilePriority::Skip) {
            return Err(EngineError::Invalid("nothing is selected: pick at least one file, or cancel the add".into()));
        }
        self.set_file_priorities(id, prios)?;
        self.apply_edge_policy(id);
        if was_held {
            self.sh.release_source(&id);
            self.forget_pending(id);
            if let Some(e) = self.table.get_mut(&id) {
                e.cold.set2(G_AWAITING, false);
            }
            self.mark_record(id);
            self.sh.emit(EngineEvent::ChoiceResolved { id, confirmed: true });
            self.replan();
            self.reconcile(id);
            if let Some(e) = self.table.get(&id) {
                let st = e.cold.state(e.live.as_ref().map(|l| &*l.stats));
                self.sh.emit(EngineEvent::StateChanged { id, state: st });
            }
            if self.table.get(&id).is_some_and(|e| self.dht_ok(&e.cold)) {
                self.dht_lookup(id, true);
            }
        }
        Ok(())
    }

    /// The user cancelled the chooser of a held torrent: remove it; nothing was written.
    pub(super) fn cancel_choice(&mut self, id: TorrentId, reply: oneshot::Sender<Result<(), EngineError>>) {
        match self.table.get(&id) {
            None => {
                let _ = reply.send(Err(EngineError::NotFound));
            }
            Some(e) if !e.cold.awaiting() => {
                let _ = reply.send(Err(EngineError::Invalid("the torrent is not waiting for a choice".into())));
            }
            Some(_) => {
                // Never delete files here: the library may already hold the very files that a
                // placed torrent's destinations name.
                let (tx, rx) = oneshot::channel();
                self.sh.emit(EngineEvent::ChoiceResolved { id, confirmed: false });
                self.remove_waiting(id, false, Some(super::RemoveWait::new(1, tx)));
                tokio::spawn(async move {
                    let _ = rx.await;
                    let _ = reply.send(Ok(()));
                });
            }
        }
    }
}
