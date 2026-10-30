//! `config.toml` (`trs_state::Config`) to [`EngineConfig`], for the first start and for every hot reload.

use std::path::PathBuf;
use std::time::Duration;

use trs_state::Config;
use trs_state::config::{AfterAdd as SAfter, WatchMode as SWatch};

use crate::config::{
    AfterAdd, AskMode, SmartAddConfig, DetectionConfig, DirectConfig, EngineConfig, PortmapConfig, TierTimings,
    WatchConfig, WatchMode,
};
use crate::organize::expand_tilde;

fn limit(v: u64) -> Option<u64> {
    (v != 0).then_some(v)
}

fn after(a: trs_state::config::AfterAdd, archive: Option<&str>) -> AfterAdd {
    match a {
        SAfter::Trash => AfterAdd::Trash,
        SAfter::Leave => AfterAdd::Leave,
        SAfter::Archive => archive.map_or(AfterAdd::Leave, |d| AfterAdd::Archive(expand_tilde(d))),
    }
}

/// Map the settings onto `base`: every setting the file owns is replaced, everything else (thread
/// counts, DHT bind address, client version, test hooks, runtime state) is kept.
pub fn engine_config(cfg: &Config, base: &EngineConfig) -> EngineConfig {
    let mut e = base.clone();
    let f = &cfg.folders;
    let download = expand_tilde(&f.download_dir);

    // Where unfinished data goes, and where the torrent's own layout lives once it is finished.
    e.default_save_dir = download;
    e.incomplete_dir = Some(expand_tilde(&f.incomplete_dir));
    e.use_staging = true;
    e.detect = Some(DetectionConfig {
        skip_junk: cfg.detection.skip_junk,
        ask_when_unsure: cfg.detection.ask_when_unsure,
        confidence_threshold: cfg.detection.confidence_threshold as f32,
    });
    e.by_status = f.by_status;
    for c in trs_classify::Category::ALL {
        let o = f.categories.get(c);
        e.category_locations[c.index()] = crate::config::CategoryLocation {
            incomplete: o.incomplete_dir.as_deref().map(expand_tilde),
            finished: o.finished_dir.as_deref().map(|d| expand_tilde(d.trim_end_matches('/'))),
        };
    }
    e.smart_add = SmartAddConfig {
        mode: match cfg.transfers.ask_before_adding {
            trs_state::config::AskBeforeAdding::Never => AskMode::Never,
            trs_state::config::AskBeforeAdding::MultiFile => AskMode::Smart,
            trs_state::config::AskBeforeAdding::Always => AskMode::Always,
        },
        large_bytes: u64::from(cfg.transfers.large_torrent_gb) * 1_000_000_000,
        free_space_override: base.smart_add.free_space_override,
    };
    e.start_delay_secs = cfg.transfers.start_delay_secs;
    e.move_rate_limit = u64::from(cfg.advanced.move_limit_mib) * 1024 * 1024;
    e.developer_mode = cfg.advanced.developer_mode || base.developer_forced;
    e.torrent_after_add = after(f.torrent_after_add, f.archive_dir.as_deref());
    e.watch = f
        .watch
        .iter()
        .filter(|w| w.enabled)
        .map(|w| WatchConfig {
            path: expand_tilde(&w.path),
            mode: match w.mode {
                SWatch::Auto => WatchMode::Auto,
                SWatch::Ask => WatchMode::Ask,
            },
            after_add: after(w.after_add, w.archive_dir.as_deref().or(f.archive_dir.as_deref())),
            category: w.category,
            rule: w.rule.clone(),
            since: w.since.map(|s| std::time::UNIX_EPOCH + Duration::from_secs(s)),
        })
        .collect();

    let t = &cfg.transfers;
    e.down_limit = limit(t.download_limit);
    e.up_limit = limit(t.upload_limit);
    e.alt_down_limit = limit(t.alt.download_limit);
    e.alt_up_limit = limit(t.alt.upload_limit);
    e.alt_speed = t.alt.enabled;
    e.alt_schedule = Some(t.alt.schedule.clone());
    e.max_active_downloads = t.max_active_downloads as usize;
    e.direct = DirectConfig {
        max_connections_per_host: t.max_connections_per_host as usize,
        connections_per_download: base.direct.connections_per_download,
    };

    let n = &cfg.network;
    e.listen_port = n.listen_port;
    e.dht_enabled = n.dht;
    e.pex_enabled = n.pex;
    e.utp = n.utp;
    e.encryption = match n.encryption {
        trs_state::config::Encryption::Off => crate::config::EncryptionPolicy::Off,
        trs_state::config::Encryption::Prefer => crate::config::EncryptionPolicy::Prefer,
        trs_state::config::Encryption::Require => crate::config::EncryptionPolicy::Require,
    };
    e.max_connections = n.max_connections as usize;
    e.max_connections_per_torrent = n.max_connections_per_torrent as usize;
    e.portmap = PortmapConfig {
        enabled: n.upnp || n.natpmp,
        upnp: n.upnp,
        natpmp: n.natpmp,
        test_gateway: base.portmap.test_gateway,
    };

    let a = &cfg.advanced;
    e.tiers = TierTimings {
        hot_to_warm: Duration::from_secs(a.tiers.hot_to_warm.0),
        warm_to_cold: Duration::from_secs(a.tiers.warm_to_cold.0),
        upload_for_hot: Duration::from_secs(a.tiers.promote_upload_after.0),
        interested_for_hot: a.tiers.promote_interested_peers as usize,
    };
    e.write_cache_bytes = (a.write_cache_mib as usize) << 20;
    e.fd_cache = (a.fd_limit as usize).max(64);
    e.preallocate = a.preallocation == trs_state::config::Preallocation::Full;
    e
}

/// Settings of `new` that differ from `old` and only take effect after a restart.
pub fn restart_required(old: &EngineConfig, new: &EngineConfig) -> Vec<String> {
    let mut v = Vec::new();
    if !old.utp && new.utp {
        v.push("network.utp".to_owned());
    }
    if !old.dht_enabled && new.dht_enabled {
        v.push("network.dht".to_owned());
    }
    if old.write_cache_bytes != new.write_cache_bytes {
        v.push("advanced.write_cache_mib".to_owned());
    }
    if old.fd_cache != new.fd_cache {
        v.push("advanced.fd_limit".to_owned());
    }
    v
}

/// Seedwell's state folder for the current user (`trs_platform::dirs::state_dir`:
/// `~/Library/Application Support/Seedwell` on macOS).
pub fn default_state_dir() -> PathBuf {
    trs_platform::dirs::state_dir().unwrap_or_else(|| PathBuf::from("Seedwell"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_map_to_a_sane_engine_config() {
        let c = Config::default();
        let e = engine_config(&c, &EngineConfig::default());
        assert!(e.use_staging && e.incomplete_dir.is_some());
        assert_eq!(e.max_active_downloads, 3);
        assert_eq!(e.down_limit, None);
        assert_eq!(e.up_limit, None);
        assert_eq!(e.alt_up_limit, Some(500_000));
        assert!(!e.by_status);
        assert!(e.detect.is_some_and(|d| d.skip_junk && d.confidence_threshold > 0.79));
        assert_eq!(e.watch.len(), 1);
        assert!(e.portmap.enabled && e.portmap.upnp && e.portmap.natpmp);
        assert_eq!(e.fd_cache, 256);
    }

    #[test]
    fn limits_zero_is_unlimited_and_the_finished_folder_is_the_default_save_dir() {
        let mut c = Config::default();
        c.transfers.upload_limit = 123;
        c.folders.by_status = true;
        c.folders.download_dir = "/data/seed".into();
        let e = engine_config(&c, &EngineConfig::default());
        assert_eq!(e.up_limit, Some(123));
        assert!(e.by_status);
        assert_eq!(e.default_save_dir, PathBuf::from("/data/seed"));
    }

    #[test]
    fn the_apps_own_limit_is_kept_when_the_file_is_applied_again() {
        let base = EngineConfig { alt_speed: true, alt_by_app: true, ..EngineConfig::default() };
        let e = engine_config(&Config::default(), &base);
        assert!(!e.alt_speed, "the toggle is the file's");
        assert!(e.alt_by_app && e.alt_active(), "the app's limit (on battery) is not in the file and stays");
    }

    #[test]
    fn restart_list_names_only_what_needs_one() {
        let a = EngineConfig::default();
        let mut b = a.clone();
        b.max_active_downloads = 9;
        assert!(restart_required(&a, &b).is_empty());
        b.listen_port = 51413; // live: no restart
        assert!(restart_required(&a, &b).is_empty());
        b.fd_cache = 77;
        assert_eq!(restart_required(&a, &b), ["advanced.fd_limit"]);
    }
}
