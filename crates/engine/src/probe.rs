//! On-demand diagnostics for `seedwell debug …`: a handful of atomics and one lock read, taken only
//! when somebody asks. Nothing here runs in the background or adds a counter to a hot path.

use std::sync::atomic::Ordering;

use crate::api::PortMapStatus;
use crate::engine::EngineHandle;
use crate::ratelimit::Dir;

/// What limits and gates look like right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineProbe {
    /// Global download limiter is set.
    pub down_limited: bool,
    /// Global upload limiter is set.
    pub up_limited: bool,
    /// Favorite trackers with a shared cap.
    pub tracker_groups: usize,
    /// Verification gate: slots in total.
    pub verify_slots: usize,
    /// Verification gate: slots free right now (`verify_slots - free` torrents are hashing).
    pub verify_free: usize,
    /// Connection budget: the global cap.
    pub max_connections: usize,
    /// Connection budget: the half-open cap.
    pub max_half_open: usize,
    /// Connections open now, handshaking ones included.
    pub connections_open: usize,
    /// Handshakes pending.
    pub half_open: usize,
    /// The DHT was started.
    pub dht_running: bool,
    /// The last port-mapping answer, once the mapper has reported.
    pub portmap: Option<PortMapStatus>,
}

impl EngineHandle {
    /// A cheap read of the gates, budgets and the port mapping (diagnostics; no side effects).
    pub fn probe(&self) -> EngineProbe {
        let sh = &self.sh;
        let cfg = sh.cfg();
        EngineProbe {
            down_limited: sh.limiter.is_limited(Dir::Down),
            up_limited: sh.limiter.is_limited(Dir::Up),
            tracker_groups: sh.groups.len(),
            verify_slots: cfg.verify_slots.max(1),
            verify_free: sh.verify_slots.available_permits(),
            max_connections: cfg.max_connections,
            max_half_open: cfg.max_half_open,
            connections_open: sh.budget.connections(),
            half_open: sh.budget.half_open(),
            dht_running: sh.dht_running.load(Ordering::Relaxed),
            portmap: sh.portmap_status.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }
}

impl EngineHandle {
    /// Move the `.torrent` file a held torrent came from to the Trash (SPEC 7A.6), for `seedwell cancel`
    /// without `--keep-file`. Takes the path now (the engine forgets it when the torrent goes), then
    /// call [`EngineHandle::trash_file`] once the cancel went through. `None` without a verified file.
    pub fn held_source_for_cancel(&self, id: crate::api::TorrentId) -> Option<std::path::PathBuf> {
        self.held_source(id)
    }

    /// Put a `.torrent` file in the Trash folder (the setting's `trash_dir`, else `~/.Trash`). Only a
    /// regular `*.torrent` file; returns whether it was moved.
    pub fn trash_file(&self, path: &std::path::Path) -> bool {
        let cfg = self.sh.cfg();
        let regular = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
        let named = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent"));
        regular && named && crate::organize::finish_file(path, &crate::config::AfterAdd::Trash, cfg.trash_dir.as_deref()).is_ok()
    }
}

impl EngineHandle {
    /// Allow (or stop allowing) the torrent with this info-hash to announce to a tracker on 127.0.0.1: for the
    /// torrents `seedwell sim` adds. Call it before the add so the first announce is covered. Never reachable
    /// from a `.torrent` file; the control layer demands permission full and Developer mode.
    pub fn set_simulated(&self, id: crate::api::TorrentId, allow: bool) {
        let mut s = self.sh.simulated.lock().unwrap_or_else(|e| e.into_inner());
        if allow {
            s.insert(id);
        } else {
            s.remove(&id);
        }
    }

    /// Whether `id` was marked by [`EngineHandle::set_simulated`].
    pub fn is_simulated(&self, id: &crate::api::TorrentId) -> bool {
        self.sh.simulated.lock().unwrap_or_else(|e| e.into_inner()).contains(id)
    }
}
