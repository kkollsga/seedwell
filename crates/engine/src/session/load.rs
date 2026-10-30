//! Restoring the torrent table from the store at start-up. No data is touched: Cold torrents
//! start Cold, and are validated lazily when they wake. Everything resumes except what the user
//! paused (and what stopped at its seeding goal); the torrents that need an actor are brought up a
//! few at a time (`boot.rs`).

use std::time::Duration;

use super::cold::*;
use super::{Entry, Session, TrackerErr, restore_counters};
use crate::persist::{Placed, StoredState, StoredTorrent};
use crate::policy;
use crate::tracker_driver::{TrackerCmd, TrackerTorrent};
use crate::util::unix_secs;
use trs_tracker::TrackerUrl;

impl Session {
    pub(crate) fn load(&mut self, stored: Vec<StoredTorrent>) {
        let cfg = self.sh.cfg();
        let pol = self.sh.policy();
        let now = unix_secs() as u32;
        let mut stored = stored;
        stored.sort_by_key(|s| (s.record.queue_pos, s.record.added_at, s.record.id));
        for st in stored {
            let r = st.record;
            let counters = restore_counters(r.total_length, st.counters.done_bytes, &st.counters);
            let root = self.intern_root(r.save_path.clone());
            let stage_root = r.stage_path.clone().map_or(NONE_U32, |p| self.intern_root(p));
            let complete = r.has_metadata && r.total_length > 0 && (r.complete || st.counters.done_bytes >= r.total_length);
            let mut host = 0;
            let tiers = r.announce.clone().unwrap_or_default();
            if let Some(u) = tiers.first().and_then(|t| t.first()).and_then(|u| TrackerUrl::parse(u).ok()) {
                host = self.intern_host(u.host());
            }
            let mut c = ColdTorrent {
                id: r.id,
                name: r.name.clone().into(),
                title: r.title.clone().map(String::into_boxed_str),
                counters: counters.clone(),
                total_len: r.total_length,
                wanted_len: r.wanted_length,
                session_base: r.session_base,
                seed_goal: r.seed_goal,
                idle: r.idle.map(Box::new),
                file_prios: (!r.file_priorities.is_empty()).then(|| r.file_priorities.clone().into_boxed_slice()),
                error: None,
                magnet: r.magnet.clone().map(String::into_boxed_str),
                piece_len: r.piece_length,
                save_root: root,
                stage_root,
                added_at: r.added_at as u32,
                finished_at: r.finished_at.unwrap_or(0) as u32,
                queue_pos: r.queue_pos,
                tracker_profile: NONE_U32,
                rule: NONE_U32,
                up_limit_kib: 0,
                // A torrent never seen busy starts from when it finished (or was added).
                last_activity: (if r.last_activity != 0 { r.last_activity } else { r.finished_at.unwrap_or(r.added_at) }) as u32,
                error_since: r.error_since as u32,
                avg_up: 0,
                category: NONE_CAT,
                seeded_secs: st.counters.seeded_secs.min(u64::from(u32::MAX)) as u32,
                seed_since: 0,
                host,
                // Everything resumes on start (SPEC §10) except what the user paused, which stays
                // paused until the user resumes it. What the app stopped by itself was stored as
                // running (an older build's pause too: it ended at quit). A fault from the last run
                // is retried: if it persists it faults again. A torrent that stopped at its seeding
                // goal stays stopped.
                want: match r.state {
                    StoredState::Active | StoredState::Error(_) => Want::Active,
                    StoredState::Paused => Want::Paused,
                    StoredState::GoalReached => Want::GoalReached,
                },
                priority: r.priority,
                flags: 0,
                flags2: 0,
                flags3: 0,
            };
            c.set_category(r.category);
            c.set2(G_CAT_USER, r.category_user);
            c.set2(G_PRIO_USER, r.priority_user);
            c.set2(G_GOAL_USER, r.goal_user);
            c.set2(G_PINNED, r.pinned);
            c.set2(G_AWAITING, r.awaiting_choice);
            // A torrent held for the grace period when the app quit: held again, with a fresh
            // countdown. Nothing starts without the user seeing it.
            if r.awaiting_choice && r.pending_start {
                c.flags3 |= H3_PENDING;
                // A countdown the user stopped comes back stopped.
                if r.pending_stopped {
                    c.flags3 |= H3_COUNTDOWN_STOPPED;
                }
            }
            c.set2(G_TRACKER_ERR, r.error_tracker);
            if r.error_tracker {
                // Until a tracker answers, the failure from the last run stands.
                self.tracker_errs.insert(r.id, TrackerErr {
                        reason: crate::tracker_driver::FAILING_SHORT.into(),
                        detail: "The tracker wasn't answering when Seedwell last ran. It is tried again as soon as the torrent starts.".into(),
                        retry_at_ms: None,
                        failures: 0,
                        permanent: false,
                    });
            }
            c.set_placed(r.placed);
            // A file that exists under its clean name and its hidden one (builds before the
            // rename was made atomic could leave the real data hidden next to a mostly empty
            // visible file) must not be trusted from the resume data: check it against both.
            if r.placed == Placed::Renamed && r.has_metadata
                && let Ok(Some(pl)) = self.sh.store.load_placement(&r.id)
                    && !crate::edgefix::reconcile(&pl).is_empty() {
                        c.set2(G_RECHECK, true);
                    }
            // Profile and rule come back as small integers; the strings live in the id table.
            if let Some(p) = r.tracker_profile.as_deref() {
                c.tracker_profile = self.ids.intern(p).unwrap_or(NONE_U32);
            }
            if let Some(p) = r.rule.as_deref() {
                c.rule = self.ids.intern(p).unwrap_or(NONE_U32);
            }
            c.set(F_META, r.has_metadata);
            c.set(F_PRIVATE, r.private);
            c.set(F_COMPLETE, complete);
            c.set(F_WANTED_DONE, r.finished_at.is_some());
            c.set(F_ADDED_COMPLETE, r.added_complete);
            c.set(F_STAGED, r.staged);
            self.next_pos = self.next_pos.max(r.queue_pos + 1);
            // The profiles may have changed since the record was written: match the announce URLs now.
            if let Some(p) = &pol {
                let urls: Vec<&str> = tiers.iter().flatten().map(String::as_str).collect();
                let m = policy::match_profile(&p.profiles, urls.iter().copied());
                c.tracker_profile = m.profile.as_deref().and_then(|p| self.ids.intern(p)).unwrap_or(NONE_U32);
            }
            // A placed torrent that is still being downloaded keeps its library paths reserved.
            if r.placed == Placed::Renamed && !complete
                && let Ok(Some(pl)) = self.sh.store.load_placement(&r.id) {
                    let mut g = self.sh.reserved.lock().unwrap_or_else(|e| e.into_inner());
                    g.extend(pl.files.iter().flatten().cloned());
                }
            let register = cfg.trackers_enabled && tiers.iter().any(|t| !t.is_empty());
            if register {
                c.set(F_TRACKER_REG, true);
                self.tracker_send(TrackerCmd::Add(TrackerTorrent {
                    id: r.id,
                    private: r.private,
                    added_complete: r.added_complete,
                    tiers,
                    counters,
                    size: r.total_length,
                }));
            }
            if c.want == Want::Active && complete {
                c.seed_since = now.max(1);
            }
            // Uploads made after the ring was last saved count; the unobserved time does not age it.
            let (seeded, up) = (c.seeded_secs, c.counters.up_total());
            if let Some(ring) = c.idle.as_mut() {
                ring.rebase(seeded, up);
            }
            let pending = c.pending();
            self.table.insert(r.id, Entry { cold: c, live: None });
            if pending {
                self.arm_pending(r.id, self.restart_delay());
            }
            if let Some(p) = &pol {
                self.apply_policy_to(r.id, p);
            }
        }
        // Torrents with an interrupted move stay stopped until its journal is resolved.
        self.block_journaled();
        self.replan();
        // Torrents that need an actor wait for their turn (`boot.rs`); the rest reconcile now.
        self.boot_hold_all();
        let ids: Vec<_> = self.table.keys().copied().collect();
        for id in ids {
            self.reconcile(id);
            // Spread DHT announces over the first half hour.
            let spread = Duration::from_secs(5 + crate::util::unix_ms().wrapping_mul(2654435761).wrapping_add(id[0] as u64 * 7919) % 1500);
            self.dht_schedule_first(id, spread);
        }
        self.boot_begin();
        self.ensure_goal_job();
        self.goal_check();
        self.recover_moves();
        self.backfill_titles();
    }
}
