//! Runtime settings: applying a changed [`EngineConfig`], the alt-speed schedule, and the services
//! that follow the configuration (watch folders, port mapping).

use std::sync::Arc;
use std::time::Duration;

use std::sync::atomic::Ordering;

use super::{Job, Session, SessionMsg};
use crate::api::*;
use crate::config::EngineConfig;
use crate::torrent::{TorrentCmd, TorrentMsg};
use crate::util::{local_utc_offset, unix_secs};

/// Longest we trust a computed schedule edge (clock changes, DST, zone changes).
const ALT_RECHECK_MAX: Duration = Duration::from_secs(15 * 60);

/// `(unix seconds, UTC offset)` according to the config's clock.
pub(crate) fn now_local(cfg: &EngineConfig) -> (i64, i32) {
    match &cfg.clock {
        Some(c) => (c.0)(),
        None => {
            let u = unix_secs() as i64;
            (u, local_utc_offset(u))
        }
    }
}

fn open_at(cfg: &EngineConfig, unix: i64, offset: i32) -> bool {
    let Some(s) = &cfg.alt_schedule else { return false };
    let local = unix.saturating_add(i64::from(offset));
    let day = trs_state::Day::from_unix(unix, offset);
    let minute = (local.rem_euclid(86_400) / 60) as u16;
    s.active_at(day.weekday(), minute)
}

/// Is the schedule's window open now?
pub(crate) fn schedule_open(cfg: &EngineConfig) -> bool {
    let (u, off) = now_local(cfg);
    open_at(cfg, u, off)
}

/// Seconds until the window next opens or closes (at most 8 days ahead), from minute resolution.
pub(crate) fn next_edge(cfg: &EngineConfig) -> Option<Duration> {
    cfg.alt_schedule.as_ref().filter(|s| s.enabled)?;
    let (u, off) = now_local(cfg);
    let now_open = open_at(cfg, u, off);
    let into_minute = u.rem_euclid(60);
    for k in 1..=(8 * 24 * 60) {
        let t = u - into_minute + k * 60;
        if open_at(cfg, t, off) != now_open {
            return Some(Duration::from_secs((t - u) as u64));
        }
    }
    None
}

impl Session {
    pub(super) fn on_update_config(&mut self, f: Box<dyn FnOnce(&mut EngineConfig) + Send>) {
        let old = self.sh.cfg();
        let mut cfg = (*old).clone();
        f(&mut cfg);
        if let Err(error) = crate::guard::check_config(&cfg) {
            // A hot reload must not be the way around the guard (a default file re-applied).
            self.sh.emit(EngineEvent::ConfigRejected { file: "config.toml".into(), error });
            return;
        }
        self.apply_cfg(&old, cfg);
    }

    fn apply_cfg(&mut self, old: &Arc<EngineConfig>, mut cfg: EngineConfig) {
        cfg.alt_scheduled_on = cfg.alt_schedule.as_ref().is_some_and(|s| s.enabled) && schedule_open(&cfg);
        let watch_changed = cfg.watch != old.watch;
        let portmap_changed = cfg.portmap != old.portmap || cfg.dht_enabled != old.dht_enabled;
        let alt_changed = cfg.alt_schedule != old.alt_schedule;
        let alt_before = old.alt_active();
        let direct_changed = cfg.direct != old.direct;
        let locations_changed = cfg.default_save_dir != old.default_save_dir
            || cfg.incomplete_dir != old.incomplete_dir
            || cfg.category_locations != old.category_locations
            || cfg.by_status != old.by_status;
        self.sh.set_cfg(cfg.clone());
        if cfg.developer_mode && !old.developer_mode {
            // The developer log names torrents by title: hand over the ones already known.
            for (id, e) in &self.table {
                if let Some(t) = &e.cold.title {
                    self.tracker_send(crate::tracker_driver::TrackerCmd::Title(*id, t.to_string()));
                }
            }
        }
        if cfg.encryption != old.encryption {
            self.tracker_send(crate::tracker_driver::TrackerCmd::Crypto(cfg.encryption));
        }
        for e in self.table.values() {
            if let Some(l) = &e.live {
                let _ = l.tx.try_send(TorrentMsg::Cmd(TorrentCmd::ConfigChanged));
            }
        }
        if cfg.alt_active() != alt_before {
            self.sh.emit(EngineEvent::AltSpeed { on: cfg.alt_active(), scheduled: cfg.alt_scheduled_on && !cfg.alt_speed });
        }
        if alt_changed {
            self.schedule_alt_check();
        }
        if watch_changed {
            self.restart_watcher();
        }
        if cfg.listen_port != old.listen_port {
            self.rebind(cfg.listen_port);
        }
        if portmap_changed {
            self.restart_portmap();
        }
        if direct_changed {
            // The per-host cap is fixed when the downloader is built; new downloads get the new one.
            self.downloader = None;
        }
        self.replan();
        if locations_changed {
            // Torrents may now sit elsewhere than the settings say: tell the app (SPEC 7A.5).
            self.announce_location_changes();
        }
    }

    /// Re-evaluate the schedule and arm the next check.
    pub(super) fn alt_check(&mut self) {
        let cfg = self.sh.cfg();
        let on = cfg.alt_schedule.as_ref().is_some_and(|s| s.enabled) && schedule_open(&cfg);
        if on != cfg.alt_scheduled_on {
            let mut c = (*cfg).clone();
            c.alt_scheduled_on = on;
            let before = cfg.alt_active();
            self.sh.set_cfg(c.clone());
            for e in self.table.values() {
                if let Some(l) = &e.live {
                    let _ = l.tx.try_send(TorrentMsg::Cmd(TorrentCmd::ConfigChanged));
                }
            }
            if c.alt_active() != before {
                self.sh.emit(EngineEvent::AltSpeed { on: c.alt_active(), scheduled: on && !c.alt_speed });
            }
            self.replan();
        }
        self.schedule_alt_check();
    }

    pub(super) fn schedule_alt_check(&mut self) {
        // Only one pending check: drop an older one before arming the new time.
        self.jobs_retain(|j| *j != Job::AltCheck);
        let cfg = self.sh.cfg();
        if let Some(d) = next_edge(&cfg) {
            // A little after the edge, so the minute has certainly turned.
            self.schedule_after(d.min(ALT_RECHECK_MAX) + Duration::from_millis(500), Job::AltCheck);
        }
    }

    /// Start what follows the configuration; called once, before the session runs.
    pub(crate) fn start_services(&mut self) {
        let mut cfg = (*self.sh.cfg()).clone();
        cfg.alt_scheduled_on = cfg.alt_schedule.as_ref().is_some_and(|s| s.enabled) && schedule_open(&cfg);
        self.sh.set_cfg(cfg);
        self.schedule_alt_check();
        self.schedule_after(Duration::from_secs(600), Job::RoleCheck);
        self.schedule_after(self.sh.cfg().status_interval, Job::StatusCheck);
        self.restart_watcher();
        self.restart_portmap();
    }

    pub(super) fn restart_watcher(&mut self) {
        if let Some(w) = self.watcher.take() {
            w.abort();
        }
        let cfg = self.sh.cfg();
        let folders = crate::watch::with_baselines(&self.sh, &cfg.watch);
        if folders.is_empty() || self.shutting_down {
            return;
        }
        self.watcher = Some(crate::watch::spawn(crate::engine::EngineHandle::from_shared(self.sh.clone()), folders));
    }

    /// Live change of `network.listen_port` (`wanted`; 0 = a fresh random port). The new TCP+UDP pair
    /// is bound first, off the actor; only a successful bind swaps (see [`Self::on_rebound`]).
    fn rebind(&mut self, wanted: u16) {
        if self.shutting_down {
            return;
        }
        self.rebind_gen += 1;
        let serial = self.rebind_gen;
        let current = self.sh.port();
        if wanted != 0 && wanted == current {
            // Already listening there (a fallback that now matches the wish): nothing to bind.
            self.sh.set_fallback(None);
            return;
        }
        let mut cfg = (*self.sh.cfg()).clone();
        // A shared socket stays a socket, even if `network.utp` was switched off meanwhile.
        cfg.utp |= self.sh.utp.read().unwrap_or_else(|e| e.into_inner()).is_some();
        let tx = self.sh.session.clone();
        tokio::spawn(async move {
            let result = crate::engine::bind_ports(&cfg, wanted, true).await.map(Box::new).map_err(|e| e.to_string());
            let _ = tx.send(SessionMsg::Rebound { serial, wanted, result }).await;
        });
    }

    /// The new sockets are bound (or not). Failure keeps everything as it was and reports a fallback
    /// naming the port still in use. Success: stop the old accept loops, start the new ones, move
    /// the DHT onto the new UDP socket, point the trackers and the port mapping at the new port.
    /// TCP peers are separate sockets and stay; uTP peers stay on the old socket until they end.
    pub(super) fn on_rebound(&mut self, serial: u64, wanted: u16, result: Result<Box<crate::engine::Bound>, String>) {
        if serial != self.rebind_gen || self.shutting_down {
            return; // superseded (the bound sockets close as they drop)
        }
        let using = self.sh.port();
        let b = match result {
            Ok(b) => *b,
            Err(_) => {
                self.sh.set_fallback(Some(PortFallback {
                    wanted,
                    using,
                    message: format!("Port {wanted} is in use by another app; using {using} for now"),
                }));
                return;
            }
        };
        let crate::engine::Bound { listener, port, utp, fallback } = b;
        if let Some(l) = self.listener.take() {
            l.abort();
        }
        if let Some(l) = self.utp_listener.take() {
            l.abort();
        }
        let old_utp = self.sh.utp.write().unwrap_or_else(|e| e.into_inner()).take();
        self.sh.listen_port.store(u32::from(port), Ordering::Relaxed);
        self.sh.set_fallback(fallback);
        self.listener = Some(tokio::spawn(crate::engine::accept_loop(listener, self.sh.clone())));
        let mut feed = None;
        if let Some((sock, frx)) = utp {
            *self.sh.utp.write().unwrap_or_else(|e| e.into_inner()) = Some(sock.clone());
            self.utp_listener = Some(tokio::spawn(crate::engine::utp_accept_loop(sock, self.sh.clone())));
            feed = Some(frx);
        }
        if let Some(o) = old_utp {
            // Refuse new connections and let go of the datagram feed; open uTP streams drain on it.
            o.set_accepting(false);
            o.set_foreign_sink(None);
        }
        // The DHT: save its state, stop it, start it again on the new socket with that state.
        let new_utp = self.sh.utp.read().unwrap_or_else(|e| e.into_inner()).clone();
        if self.dht_shared
            && let (Some(old), Some(sock), Some(feed)) = (self.dht.clone(), new_utp, feed)
        {
            let sh = self.sh.clone();
            let tx = sh.session.clone();
            tokio::spawn(async move {
                let state = tokio::time::timeout(Duration::from_secs(1), old.shutdown()).await.ok().and_then(Result::ok);
                let cfg = sh.cfg();
                let dht = crate::engine::spawn_dht_shared(&cfg, &*sh.store, sock, feed, crate::engine::alive_of(&sh), state);
                let _ = tx.send(SessionMsg::DhtMoved { serial, dht }).await;
            });
        }
        // Trackers hear the new port with their next regular announce (no extra announce).
        self.tracker_send(crate::tracker_driver::TrackerCmd::ListenPort(port));
        self.sh.incoming_public.store(0, Ordering::Relaxed);
        self.sh.last_incoming_public.store(0, Ordering::Relaxed);
        self.restart_portmap();
    }

    pub(super) fn on_dht_moved(&mut self, serial: u64, dht: trs_dht::DhtHandle) {
        if self.shutting_down || serial != self.rebind_gen {
            tokio::spawn(async move { dht.shutdown().await });
            return;
        }
        self.dht = Some(dht);
    }

    pub(super) fn restart_portmap(&mut self) {
        if let Some(pm) = self.portmap.take() {
            // Mappings of the old runner are deleted in the background.
            tokio::spawn(async move { pm.shutdown().await });
        }
        // Until the new mapper answers (or for good, with mapping off) there is no status.
        self.portmap_gen += 1;
        *self.sh.portmap_status.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if self.shutting_down {
            return;
        }
        self.portmap = crate::portmap::PortMapRunner::start(&self.sh, self.portmap_gen, self.extra_mapped_port);
    }

    /// Folder sync's port, mapped next to the listen port (or no longer). Only a change restarts the
    /// mapper. The new one starts once the old one has deleted its mappings: both ask for the listen
    /// port, and a late delete from the old one must not undo the new one's mapping.
    pub(super) fn set_extra_mapped_port(&mut self, port: Option<u16>) {
        let port = port.filter(|&p| p != 0);
        if port == self.extra_mapped_port {
            return;
        }
        self.extra_mapped_port = port;
        let Some(old) = self.portmap.take() else { return };
        self.portmap_gen += 1;
        let serial = self.portmap_gen;
        *self.sh.portmap_status.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let tx = self.sh.session.clone();
        tokio::spawn(async move {
            old.shutdown().await;
            let _ = tx.send(SessionMsg::PortMapRestart { serial }).await;
        });
    }

    pub(super) fn on_portmap_restart(&mut self, serial: u64) {
        // A newer (re)start or a shutdown came in meanwhile: that one decides.
        if serial != self.portmap_gen || self.shutting_down || self.portmap.is_some() {
            return;
        }
        self.portmap = crate::portmap::PortMapRunner::start(&self.sh, self.portmap_gen, self.extra_mapped_port);
    }

    pub(super) fn on_portmap(&mut self, serial: u64, s: PortMapStatus) {
        if serial != self.portmap_gen {
            return;
        }
        // Trackers must be given the port peers can reach: the mapped external TCP port when the
        // router granted another one than we asked for. Start-up waits for this (bounded).
        if s.external_tcp_port.is_some() || s.unavailable {
            let listen = self.sh.port();
            self.tracker_send(crate::tracker_driver::TrackerCmd::PortKnown(
                s.external_tcp_port.filter(|&p| p != 0 && p != listen),
            ));
        }
        *self.sh.portmap_status.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.clone());
        self.sh.emit(EngineEvent::PortMap(s));
    }

    /// `trackers`: also pull the retries of announces that failed (not after an outage: the hub
    /// has released exactly the announces that came due by itself).
    pub(super) fn on_network_changed(&mut self, trackers: bool) {
        if trackers {
            self.tracker_send(crate::tracker_driver::TrackerCmd::NetworkChanged);
        }
        if let Some(pm) = &self.portmap {
            pm.network_changed();
        }
    }

    /// The hub holds everything (an outage, or the short check after a wake) or lets go of it
    /// (SPEC 7C.4c). Holding pauses the DHT; letting go resumes it with a fresh bootstrap
    /// backoff, tells the torrents with nobody connected to forget their dial backoff, and does
    /// the network-change work once (port mapping, listen sockets).
    pub(super) fn on_net(&mut self, held: bool) {
        if let Some(d) = self.dht.clone() {
            tokio::spawn(async move {
                let _ = if held { d.set_paused(true).await } else { d.network_restored().await };
            });
        }
        if held {
            return;
        }
        for e in self.table.values() {
            if let Some(l) = &e.live {
                let _ = l.tx.try_send(TorrentMsg::NetRestored);
            }
        }
        self.on_network_changed(false);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{AltSchedule, ClockFn};
    use trs_state::config::{DaySet, TimeOfDay};

    /// 2026-10-12 was a Monday.
    fn monday(h: i64, m: i64, s: i64) -> i64 {
        trs_state::fsutil::days_from_civil(2026, 10, 12) * 86_400 + h * 3600 + m * 60 + s
    }

    fn cfg_at(t: i64, off: i32, days: DaySet, from: u16, to: u16) -> EngineConfig {
        EngineConfig {
            alt_schedule: Some(AltSchedule { enabled: true, days, from: TimeOfDay(from), to: TimeOfDay(to) }),
            clock: Some(ClockFn(Arc::new(move || (t, off)))),
            ..EngineConfig::default()
        }
    }

    #[test]
    fn the_next_edge_is_found_to_the_minute() {
        let w = DaySet::WEEKDAYS;
        // Monday 07:00:10, window 08:00-17:00: opens in 59 min 50 s (the minute turns at 08:00:00).
        assert_eq!(next_edge(&cfg_at(monday(7, 0, 10), 0, w, 480, 1020)), Some(Duration::from_secs(3590)));
        // Inside at 09:00:30: closes at 17:00:00.
        assert_eq!(next_edge(&cfg_at(monday(9, 0, 30), 0, w, 480, 1020)), Some(Duration::from_secs(8 * 3600 - 30)));
        // Friday 17:30: the next edge is Monday 08:00 (62.5 h later).
        let friday = monday(17, 30, 0) + 4 * 86_400;
        assert_eq!(next_edge(&cfg_at(friday, 0, w, 480, 1020)), Some(Duration::from_secs(62 * 3600 + 1800)));
        // A disabled schedule has no edges.
        let mut c = cfg_at(monday(7, 0, 0), 0, w, 480, 1020);
        c.alt_schedule.as_mut().unwrap().enabled = false;
        assert_eq!(next_edge(&c), None);
        assert!(!schedule_open(&c));
    }

    #[test]
    fn the_zone_offset_moves_the_window() {
        // 07:00 UTC is 09:00 in a zone two hours ahead: inside an 08:00-17:00 window.
        let c = cfg_at(monday(7, 0, 0), 7200, DaySet::WEEKDAYS, 480, 1020);
        assert!(schedule_open(&c));
        let c = cfg_at(monday(7, 0, 0), 0, DaySet::WEEKDAYS, 480, 1020);
        assert!(!schedule_open(&c));
        // Local midnight belongs to the next weekday: Sunday 23:00 UTC is Monday 01:00 at +2 h.
        let sunday_2300 = monday(0, 0, 0) - 3600;
        let c = cfg_at(sunday_2300, 7200, DaySet(1), 0, 120);
        assert!(schedule_open(&c), "Monday 01:00 local, Monday-only window 00:00-02:00");
    }
}
