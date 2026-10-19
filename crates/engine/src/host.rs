//! The whole stack from a state directory: `state.redb`, `config.toml`, `rules.toml`,
//! `trackers.toml`, hot reload, and the engine on top. This is what the app, the CLI daemon and
//! the benchmark binary start.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use trs_state::doc::POLL_INTERVAL;
use trs_state::{Config, DocStore, PollEvent, Poller, RuleSet, TrackerProfiles, WatchHandle, spawn_watcher};

use crate::api::{EngineError, EngineEvent, ShutdownReport};
use crate::config::{ClockFn, EngineConfig};
use crate::config_map;
use crate::engine::{Engine, EngineHandle};
use crate::policy::PolicyData;
use crate::state_store::StateStore;

/// Adjusts the engine configuration before it starts.
pub type ConfigTweak = Box<dyn FnOnce(&mut EngineConfig) + Send>;

/// How to start a [`Host`].
pub struct HostOptions {
    /// Adjust the engine configuration derived from `config.toml` before the engine starts
    /// (tests: loopback, small caches; the benchmark binary: no queueing).
    pub tweak: Option<ConfigTweak>,
    /// The app's time zone (`unix seconds -> UTC offset`), used for statistics days and the
    /// alt-speed schedule. `None` asks `/bin/date`.
    pub time_zone: Option<Arc<dyn Fn(i64) -> i32 + Send + Sync>>,
    /// Poll the three settings files for edits.
    pub hot_reload: bool,
    /// How often (tests use a short one).
    pub poll_interval: Duration,
    /// Developer mode forced on for this process, whatever `config.toml` says (`seedwell daemon --dev-mode`).
    pub developer_mode: bool,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self { tweak: None, time_zone: None, hot_reload: true, poll_interval: POLL_INTERVAL, developer_mode: false }
    }
}

#[derive(Default)]
struct Docs {
    cfg: Config,
    rules: RuleSet,
    profiles: TrackerProfiles,
}

/// A running engine plus everything that keeps its settings and state in step with the disk.
pub struct Host {
    engine: Engine,
    store: Arc<StateStore>,
    dir: PathBuf,
    watchers: Vec<WatchHandle>,
    /// The runtime of [`Host::start_blocking`]; `None` when started on the caller's runtime.
    runtime: Option<tokio::runtime::Runtime>,
    /// What opening the database found (created, recovered, migrated).
    pub open_report: trs_state::OpenReport,
    /// Problems found while loading the settings files (the files in force are valid anyway).
    pub warnings: Vec<String>,
}

/// The baseline stamp for "now": rounded *up* to the next whole second, so that a file that was
/// in the folder a moment ago is never newer than the baseline because of the rounding.
fn baseline_now() -> u64 {
    crate::util::unix_secs() + 1
}

fn msgs<'a, I: IntoIterator<Item = &'a trs_state::Warning>>(w: I) -> Vec<String> {
    w.into_iter().map(ToString::to_string).collect()
}

impl Host {
    /// Open `dir`, load the settings, start the engine and the hot-reload watchers.
    pub async fn start(dir: impl AsRef<Path>, opts: HostOptions) -> Result<Host, EngineError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| EngineError::Io(format!("{}: {e}", dir.display())))?;
        let opened = trs_state::Store::open(&dir).map_err(|e| EngineError::Store(e.to_string()))?;
        let report = opened.report.clone();
        let store = match opts.time_zone.clone() {
            Some(tz) => StateStore::with_time_zone(opened.store, move |t| tz(t)),
            None => StateStore::new(opened.store),
        };

        let mut warnings = Vec::new();
        let cfg_docs = DocStore::<Config>::new(&dir);
        let rules_docs = DocStore::<RuleSet>::new(&dir);
        let prof_docs = DocStore::<TrackerProfiles>::new(&dir);
        let (lc, lr, lp) = (cfg_docs.load(), rules_docs.load(), prof_docs.load());
        for (name, err, w) in [
            ("config.toml", &lc.error, &lc.warnings),
            ("rules.toml", &lr.error, &lr.warnings),
            ("trackers.toml", &lp.error, &lp.warnings),
        ] {
            if let Some(e) = err {
                warnings.push(format!("{name}: {e}"));
            }
            warnings.extend(msgs(w).into_iter().map(|m| format!("{name}: {m}")));
        }
        let mut docs = Docs { cfg: lc.value, rules: lr.value, profiles: lp.value };
        // Watch folders only pick up files that arrive after they joined the list (SPEC 7A.1):
        // stamp the entries that have no baseline yet (first launch, upgrade) and remember it.
        if docs.cfg.stamp_watch_baselines(None, baseline_now()) && lc.error.is_none()
            && let Err(e) = cfg_docs.save(&docs.cfg) {
                warnings.push(format!("config.toml: cannot save the watch folder baselines: {e}"));
            }
        warnings.extend(msgs(&trs_state::cross_check(&docs.cfg, &docs.rules, &docs.profiles)));

        let mut base = EngineConfig::default();
        if let Some(tz) = &opts.time_zone {
            let tz = tz.clone();
            base.clock = Some(ClockFn(Arc::new(move || {
                let u = crate::util::unix_secs() as i64;
                (u, tz(u))
            })));
        }
        base.devlog_dir = Some(dir.join("logs"));
        base.developer_forced = opts.developer_mode;
        let mut ec = config_map::engine_config(&docs.cfg, &base);
        // The hot-reload watchers share one copy of the documents.
        let shared = Arc::new(Mutex::new(Docs { cfg: docs.cfg.clone(), rules: docs.rules.clone(), profiles: docs.profiles.clone() }));
        if let Some(t) = opts.tweak {
            t(&mut ec);
        }
        crate::guard::check_config(&ec).map_err(EngineError::Io)?;
        crate::guard::check_profiles(&docs.profiles).map_err(EngineError::Io)?;
        if report.recovered.is_some() {
            // The database was damaged and set aside: rebuild the torrent list from `torrents/`.
            let n = store.rebuild_from_torrent_files(&ec.default_save_dir).unwrap_or(0);
            warnings.push(format!("state.redb was damaged; {n} torrents were rebuilt from their .torrent files"));
        }
        let engine = Engine::start_with_policy(
            ec,
            store.clone(),
            PolicyData::new(docs.rules.clone(), docs.profiles.clone(), docs.cfg.transfers.private_min_seed_secs).with_idle(&docs.cfg.transfers),
        )
        .await?;
        let h = engine.handle();

        // `network.listen_port = 0` means a random port at every start: it is never written back.

        let mut watchers = Vec::new();
        if opts.hot_reload {
            let rt = tokio::runtime::Handle::current();
            if let Ok(mut d) = shared.lock() {
                d.cfg = docs.cfg.clone();
            }
            let iv = opts.poll_interval;
            {
                let (h, rt, shared) = (h.clone(), rt.clone(), shared.clone());
                let running = shared.lock().map(|d| d.cfg.clone()).unwrap_or_default();
                let cfg_save = cfg_docs.clone();
                let p = Poller::new(cfg_docs, running);
                watchers.extend(spawn_watcher(p, iv, move |ev| match ev {
                    PollEvent::Reloaded(mut parsed) => {
                        let (h, shared, cfg_save) = (h.clone(), shared.clone(), cfg_save.clone());
                        // A watch folder that is new (or was switched on again) starts its baseline
                        // now; the stamped file is saved so the baseline survives a restart.
                        let prev = shared.lock().map(|d| d.cfg.clone()).unwrap_or_default();
                        if parsed.value.stamp_watch_baselines(Some(&prev), baseline_now()) {
                            let _ = cfg_save.save(&parsed.value);
                        }
                        rt.spawn(async move {
                            let (warnings, policy) = {
                                let mut d = shared.lock().unwrap_or_else(|e| e.into_inner());
                                d.cfg = parsed.value.clone();
                                let mut w = msgs(&parsed.warnings);
                                w.extend(msgs(&trs_state::cross_check(&d.cfg, &d.rules, &d.profiles)));
                                (w, PolicyData::new(d.rules.clone(), d.profiles.clone(), d.cfg.transfers.private_min_seed_secs).with_idle(&d.cfg.transfers))
                            };
                            // The private floor lives in config.toml but acts through the policy.
                            h.apply_policy(policy);
                            match h.apply_settings(&parsed.value).await {
                                Ok(restart) => h.publish(EngineEvent::ConfigReloaded {
                                    file: "config.toml".into(),
                                    warnings,
                                    restart_required: restart,
                                }),
                                Err(e) => h.publish(EngineEvent::ConfigRejected { file: "config.toml".into(), error: e.to_string() }),
                            }
                        });
                    }
                    PollEvent::Rejected(e) => {
                        h.publish(EngineEvent::ConfigRejected { file: "config.toml".into(), error: e.to_string() });
                    }
                }).ok());
            }
            {
                let (h, rt, shared) = (h.clone(), rt.clone(), shared.clone());
                let running = shared.lock().map(|d| d.rules.clone()).unwrap_or_default();
                let p = Poller::new(rules_docs, running);
                watchers.extend(spawn_watcher(p, iv, move |ev| match ev {
                    PollEvent::Reloaded(parsed) => {
                        let (h, shared) = (h.clone(), shared.clone());
                        rt.spawn(async move {
                            let (policy, warnings) = {
                                let mut d = shared.lock().unwrap_or_else(|e| e.into_inner());
                                d.rules = parsed.value.clone();
                                let mut w = msgs(&parsed.warnings);
                                w.extend(msgs(&trs_state::cross_check(&d.cfg, &d.rules, &d.profiles)));
                                (PolicyData::new(d.rules.clone(), d.profiles.clone(), d.cfg.transfers.private_min_seed_secs).with_idle(&d.cfg.transfers), w)
                            };
                            h.apply_policy(policy);
                            h.publish(EngineEvent::ConfigReloaded { file: "rules.toml".into(), warnings, restart_required: Vec::new() });
                        });
                    }
                    PollEvent::Rejected(e) => {
                        h.publish(EngineEvent::ConfigRejected { file: "rules.toml".into(), error: e.to_string() });
                    }
                }).ok());
            }
            {
                let (h, rt, shared) = (h.clone(), rt.clone(), shared.clone());
                let running = shared.lock().map(|d| d.profiles.clone()).unwrap_or_default();
                let p = Poller::new(prof_docs, running);
                watchers.extend(spawn_watcher(p, iv, move |ev| match ev {
                    PollEvent::Reloaded(parsed) => {
                        let (h, shared) = (h.clone(), shared.clone());
                        rt.spawn(async move {
                            let (policy, warnings) = {
                                let mut d = shared.lock().unwrap_or_else(|e| e.into_inner());
                                d.profiles = parsed.value.clone();
                                let mut w = msgs(&parsed.warnings);
                                w.extend(msgs(&trs_state::cross_check(&d.cfg, &d.rules, &d.profiles)));
                                (PolicyData::new(d.rules.clone(), d.profiles.clone(), d.cfg.transfers.private_min_seed_secs).with_idle(&d.cfg.transfers), w)
                            };
                            h.apply_policy(policy);
                            h.publish(EngineEvent::ConfigReloaded { file: "trackers.toml".into(), warnings, restart_required: Vec::new() });
                        });
                    }
                    PollEvent::Rejected(e) => {
                        h.publish(EngineEvent::ConfigRejected { file: "trackers.toml".into(), error: e.to_string() });
                    }
                }).ok());
            }
        }
        Ok(Host { engine, store, dir, watchers, runtime: None, open_report: report, warnings })
    }

    /// [`Host::start`] on a private runtime, for callers that have none (the FFI layer, the CLI).
    pub fn start_blocking(dir: impl AsRef<Path>, opts: HostOptions) -> Result<Host, EngineError> {
        let rt = crate::engine::build_runtime().map_err(|e| EngineError::Io(e.to_string()))?;
        let mut host = rt.block_on(Self::start(dir, opts))?;
        host.runtime = Some(rt);
        Ok(host)
    }

    /// Blocking form of [`Host::shutdown`]; also stops the private runtime.
    pub fn shutdown_blocking(mut self, deadline: Duration) -> ShutdownReport {
        let rt = self.runtime.take();
        match rt {
            Some(rt) => {
                let r = rt.block_on(self.shutdown(deadline));
                rt.shutdown_timeout(Duration::from_secs(2));
                r
            }
            None => {
                // Started on the caller's runtime: there is nothing to block on here.
                ShutdownReport::default()
            }
        }
    }

    /// The control handle.
    pub fn handle(&self) -> EngineHandle {
        self.engine.handle()
    }

    /// The state store (statistics queries, backups).
    pub fn store(&self) -> &Arc<StateStore> {
        &self.store
    }

    /// The state directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stop the watchers, close the tracker sessions (bounded by `deadline`), delete port
    /// mappings, persist everything.
    pub async fn shutdown(self, deadline: Duration) -> ShutdownReport {
        let Host { engine, store, watchers, runtime, .. } = self;
        for w in watchers {
            w.stop();
        }
        let r = engine.shutdown(deadline).await;
        let _ = store.state().flush_now();
        if let Some(rt) = runtime {
            // Only when the async form is used on a blocking-started host; dropping a runtime
            // inside async code would panic.
            rt.shutdown_background();
        }
        r
    }
}
