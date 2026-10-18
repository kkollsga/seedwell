//! Applying rules and tracker profiles to torrents that are already in the table, re-matching
//! profiles after `trackers.toml` changed, and the work that follows completion
//! (releasing reserved paths).

use std::sync::Arc;

use trs_classify::Category;
use trs_metainfo::{Magnet, Metainfo};

use super::cold::*;
use super::{Session, SessionMsg};
use crate::api::*;
use crate::organize;
use crate::policy::{self, PolicyData, ProfileMatch};
use crate::torrent::{TorrentCmd, TorrentMsg};

impl Session {
    /// Recompute the rule-derived fields of one torrent. Returns true if anything changed.
    pub(crate) fn apply_policy_to(&mut self, id: TorrentId, pol: &PolicyData) -> bool {
        let Some(e) = self.table.get_mut(&id) else { return false };
        let c = &mut e.cold;
        let profile = ColdTorrent::opt(c.tracker_profile)
            .and_then(|i| self.ids.get(i))
            .filter(|p| pol.profiles.get(p).is_some());
        let m = ProfileMatch {
            profile: profile.map(str::to_owned),
            linked_rule: profile.and_then(|p| pol.profiles.get(p)).and_then(|p| p.rule.clone()),
        };
        let forced = ColdTorrent::opt(c.rule).and_then(|i| self.ids.get(i));
        let r = policy::resolve(&pol.rules, &m, forced, c.private(), c.category().unwrap_or(Category::Other), pol.private_min_seed, pol.idle);
        let mut changed = false;
        if !c.has2(G_PRIO_USER) && c.priority != r.priority.to_u8() {
            c.priority = r.priority.to_u8();
            if let Some(l) = &e.live {
                let _ = l.tx.try_send(TorrentMsg::Cmd(TorrentCmd::SetPriority(r.priority)));
            }
            changed = true;
        }
        // A user-chosen goal keeps its kind; the hit-and-run floor and the action at goal always
        // come from the rule.
        let goal = if c.has2(G_GOAL_USER) {
            SeedGoal { min_seed_secs: r.goal.min_seed_secs, at_goal: r.goal.at_goal, idle_mb: r.goal.idle_mb, idle_hours: r.goal.idle_hours, ..c.seed_goal }
        } else {
            r.goal
        };
        if goal != c.seed_goal {
            c.seed_goal = goal;
            changed = true;
        }
        let before = c.up_limit_kib;
        c.set_up_limit(r.up_limit);
        if c.up_limit_kib != before {
            if let Some(l) = &e.live {
                let _ = l.tx.try_send(TorrentMsg::Cmd(TorrentCmd::SetUpLimit(c.up_limit())));
            }
            changed = true;
        }
        // The tracker's shared caps: a live torrent joins, changes or leaves its group.
        if let Some(l) = &e.live {
            let group = profile.and_then(|p| self.sh.groups.get(p));
            let _ = l.tx.try_send(TorrentMsg::Cmd(TorrentCmd::SetGroup(group)));
        }
        changed
    }

    /// Install new rules and profiles; every torrent is re-evaluated (SPEC 7.1: "re-evaluated
    /// live when edited").
    pub(crate) fn set_policy(&mut self, pol: Option<Arc<PolicyData>>) {
        // The handle already published `pol` for adds that follow; this is the re-evaluation.
        let old = std::mem::replace(&mut self.applied_policy, pol.clone());
        // Bytes counted so far belong to the profiles that were in force.
        self.final_stats();
        self.sh.set_policy(pol.clone());
        let Some(pol) = pol else { return };
        let ids: Vec<TorrentId> = self.table.keys().copied().collect();
        for id in ids {
            if self.apply_policy_to(id, &pol) {
                self.mark_record(id);
            }
        }
        self.replan();
        self.goal_check();
        // Only a change of ids or addresses can move a torrent between trackers; a renamed, recoloured
        // or re-capped favorite keeps every torrent where it is.
        if old.as_deref().is_none_or(|o| !same_matching(&o.profiles, &pol.profiles)) {
            self.rematch_profiles(pol);
        }
    }

    /// Match every torrent's announce URLs against the new profiles, off the actor.
    fn rematch_profiles(&mut self, pol: Arc<PolicyData>) {
        let jobs: Vec<(TorrentId, bool, Option<String>)> = self
            .table
            .iter()
            .map(|(id, e)| (*id, e.cold.has_metadata(), e.cold.magnet.as_deref().map(str::to_owned)))
            .collect();
        let store = self.sh.store.clone();
        let sh = self.sh.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::with_capacity(jobs.len());
            for (id, has_meta, magnet) in jobs {
                let urls: Vec<String> = if has_meta {
                    match store.load_torrent_file(&id).ok().flatten().and_then(|b| Metainfo::from_bytes(&b).ok()) {
                        Some(mi) => mi.announce_tiers.iter().flat_map(|t| t.iter().map(|u| u.to_string())).collect(),
                        None => continue,
                    }
                } else {
                    magnet.and_then(|m| Magnet::parse(&m).ok()).map(|m| m.trackers).unwrap_or_default()
                };
                out.push((id, policy::match_profile(&pol.profiles, urls.iter().map(String::as_str))));
            }
            let _ = sh.session.blocking_send(SessionMsg::Rematched(out));
        });
    }

    pub(super) fn on_rematched(&mut self, v: Vec<(TorrentId, ProfileMatch)>) {
        let Some(pol) = self.sh.policy() else { return };
        self.final_stats();
        for (id, m) in v {
            let want = m.profile.as_deref().and_then(|p| self.ids.intern(p)).unwrap_or(NONE_U32);
            let Some(e) = self.table.get_mut(&id) else { continue };
            let mut changed = false;
            if e.cold.tracker_profile != want {
                e.cold.tracker_profile = want;
                changed = true;
            }
            changed |= self.apply_policy_to(id, &pol);
            if changed {
                self.mark_record(id);
            }
        }
        self.replan();
        self.goal_check();
    }

    pub(super) fn set_category(&mut self, ids: Vec<TorrentId>, cat: Option<Category>) {
        let pol = self.sh.policy();
        let mut relocate = Vec::new();
        let mut retitle = Vec::new();
        for id in ids {
            if self.directs.contains_key(&id) {
                self.direct_set_category(id, cat);
                continue;
            }
            let Some(e) = self.table.get_mut(&id) else { continue };
            let changed = e.cold.category() != cat;
            // A new category means a new place (SPEC 7A.2: "this re-applies placement and rule"),
            // unless the user chose where this torrent lives.
            if changed && !e.cold.has2(G_PINNED) && e.cold.has_metadata() && !e.cold.awaiting() {
                relocate.push(id);
            }
            e.cold.set_category(cat);
            if changed && e.cold.has_metadata() {
                retitle.push((id, cat));
            }
            // `None` hands the choice back to detection (applied again on the next re-add).
            e.cold.set2(G_CAT_USER, cat.is_some());
            if let Some(p) = &pol {
                self.apply_policy_to(id, p);
            }
            self.mark_record(id);
        }
        self.retitle(retitle);
        self.replan();
        self.goal_check();
        if !relocate.is_empty() {
            self.queue_moves(relocate, crate::moveplan::MoveTarget::Config);
        }
    }

    pub(super) fn set_rule(&mut self, ids: Vec<TorrentId>, rule: Option<String>) {
        let pol = self.sh.policy();
        let rid = rule.as_deref().and_then(|r| self.ids.intern(r)).unwrap_or(NONE_U32);
        for id in ids {
            let Some(e) = self.table.get_mut(&id) else { continue };
            e.cold.rule = rid;
            if let Some(p) = &pol {
                self.apply_policy_to(id, p);
            }
            self.mark_record(id);
        }
        self.replan();
        self.goal_check();
    }

    // ---- placement follow-up -----------------------------------------------------------------

    /// Forget the library paths reserved for `id`: its files are in place now (or it is gone).
    pub(crate) fn release_reservations(&mut self, id: TorrentId) {
        if self.table.get(&id).is_some_and(|e| e.cold.has2(G_PLACED)) {
            if let Ok(Some(p)) = self.sh.store.load_placement(&id) {
                organize::release(&self.sh.reserved, &p);
            }
        }
    }
}

/// The profiles match the same torrents: same ids in the same order with the same addresses.
fn same_matching(a: &trs_state::TrackerProfiles, b: &trs_state::TrackerProfiles) -> bool {
    a.profiles.len() == b.profiles.len() && a.profiles.iter().zip(&b.profiles).all(|(x, y)| x.id == y.id && x.urls == y.urls)
}
