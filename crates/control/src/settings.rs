//! Allow-listed settings access. An agent may read and change folders, limits, queue and
//! notification settings (SPEC 9A.2). Never reachable: `agents.*` (an agent must not change its
//! own permission), `network.*`, `advanced.*` except `advanced.developer_mode`, watch folders, launch-at-login. Client identity
//! and tracker behaviour are not settings at all (SPEC 7B.1).

use serde_json::{Map, Value};
use trs_state::Config;

use crate::proto::{ApiError, ErrorCode};

/// Every key `settings_get` and `settings_set` accept.
pub const ALLOWED: &[&str] = &[
    "general.notify_finished",
    "general.notify_errors",
    "general.notify_goal",
    "general.prevent_sleep_while_downloading",
    "general.on_battery",
    "transfers.download_limit",
    "transfers.upload_limit",
    "transfers.max_active_downloads",
    "transfers.max_connections_per_host",
    "transfers.ask_before_adding",
    "transfers.private_min_seed_secs",
    "transfers.idle_min_upload_bytes",
    "transfers.idle_window_hours",
    "transfers.alt.enabled",
    "transfers.alt.download_limit",
    "transfers.alt.upload_limit",
    "transfers.alt.schedule.enabled",
    "transfers.alt.schedule.days",
    "transfers.alt.schedule.from",
    "transfers.alt.schedule.to",
    "folders.download_dir",
    "folders.incomplete_dir",
    "folders.by_status",
    "folders.torrent_after_add",
    "folders.archive_dir",
    "detection.skip_junk",
    "detection.ask_when_unsure",
    "detection.confidence_threshold",
    // Developer mode logs every tracker exchange (passkeys masked) for `seedwell debug trackers log`.
    "advanced.developer_mode",
];

fn lookup<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(root, |v, part| v.get(part))
}

fn set_path(root: &mut Value, key: &str, new: Value) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, parents)) = parts.split_last() else { return false };
    let mut cur = root;
    for p in parents {
        match cur.get_mut(*p) {
            Some(next) => cur = next,
            None => return false,
        }
    }
    match cur.as_object_mut() {
        Some(o) => {
            o.insert((*last).to_owned(), new);
            true
        }
        None => false,
    }
}

/// The requested keys (default: all allowed) with their current values.
pub fn get(cfg: &Config, keys: Option<&[String]>) -> Result<Map<String, Value>, ApiError> {
    let root = serde_json::to_value(cfg).map_err(|e| ApiError::internal(format!("settings: {e}")))?;
    let mut out = Map::new();
    let wanted: Vec<&str> = match keys {
        None => ALLOWED.to_vec(),
        Some(ks) => {
            let mut v = Vec::with_capacity(ks.len());
            for k in ks.iter().take(100) {
                if !ALLOWED.contains(&k.as_str()) {
                    return Err(not_allowed(k));
                }
                v.push(k.as_str());
            }
            v
        }
    };
    for k in wanted {
        out.insert(k.to_owned(), lookup(&root, k).cloned().unwrap_or(Value::Null));
    }
    Ok(out)
}

fn not_allowed(key: &str) -> ApiError {
    ApiError::new(
        ErrorCode::InvalidParams,
        format!("setting `{}` is not available to agents", crate::proto::clip(key, 80)),
    )
    .with_data(serde_json::json!({ "allowed": ALLOWED }))
}

/// Apply `changes` to a copy of `cfg`. Returns the new config (validated) and the changed keys.
/// All-or-nothing: any bad key or value rejects the whole change with field-path errors.
pub fn apply(cfg: &Config, changes: &Map<String, Value>) -> Result<(Config, Vec<String>), ApiError> {
    if changes.is_empty() {
        return Err(ApiError::params("no changes given"));
    }
    if changes.len() > 64 {
        return Err(ApiError::params("more than 64 changes in one call"));
    }
    let mut root = serde_json::to_value(cfg).map_err(|e| ApiError::internal(format!("settings: {e}")))?;
    let mut problems: Vec<Value> = Vec::new();
    let mut changed = Vec::new();
    for (k, v) in changes {
        if !ALLOWED.contains(&k.as_str()) {
            return Err(not_allowed(k));
        }
        let mut trial = root.clone();
        if !set_path(&mut trial, k, v.clone()) {
            return Err(ApiError::internal(format!("settings path {k} does not exist")));
        }
        // Type errors are attributed to the key that caused them.
        if let Err(e) = serde_json::from_value::<Config>(trial.clone()) {
            problems.push(serde_json::json!({ "path": k, "reason": crate::proto::clip(&e.to_string(), 160) }));
            continue;
        }
        root = trial;
        changed.push(k.clone());
    }
    if !problems.is_empty() {
        return Err(ApiError::invalid("some values have the wrong type").with_data(serde_json::json!({ "errors": problems })));
    }
    let mut new: Config = serde_json::from_value(root).map_err(|e| ApiError::invalid(e.to_string()))?;
    new.unknown = cfg.unknown.clone();
    let errs = new.validate();
    if !errs.is_empty() {
        let list: Vec<Value> = errs.iter().map(|e| serde_json::json!({ "path": e.path, "reason": e.reason })).collect();
        let first = errs.first().map(|e| format!("{}: {}", e.path, e.reason)).unwrap_or_default();
        return Err(ApiError::invalid(format!("settings rejected: {first}")).with_data(serde_json::json!({ "errors": list })));
    }
    Ok((new, changed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn changes(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn private_min_seed_is_a_setting_with_a_ten_day_default() {
        let all = get(&Config::default(), None).unwrap();
        assert_eq!(all["transfers.private_min_seed_secs"], json!(864_000));
        let (c, ch) = apply(&Config::default(), &changes(json!({"transfers.private_min_seed_secs": 0}))).unwrap();
        assert_eq!((c.transfers.private_min_seed_secs, ch.len()), (0, 1));
    }

    #[test]
    fn get_returns_allowed_keys_only() {
        let all = get(&Config::default(), None).unwrap();
        assert_eq!(all.len(), ALLOWED.len());
        assert!(all.contains_key("transfers.download_limit"));
        assert!(!all.keys().any(|k| k.starts_with("agents") || k.starts_with("network")));
        assert!(get(&Config::default(), Some(&["agents.permission".into()])).is_err());
    }

    #[test]
    fn set_applies_and_validates() {
        let (c, ch) = apply(&Config::default(), &changes(json!({"transfers.download_limit": 5_000_000, "folders.by_status": true}))).unwrap();
        assert_eq!(c.transfers.download_limit, 5_000_000);
        assert_eq!(ch.len(), 2);
        let e = apply(&Config::default(), &changes(json!({"transfers.max_active_downloads": 0}))).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid);
        assert!(e.message.contains("max_active_downloads"), "{}", e.message);
        let e = apply(&Config::default(), &changes(json!({"transfers.download_limit": "fast"}))).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid);
        assert!(e.data.unwrap().to_string().contains("transfers.download_limit"));
    }

    #[test]
    fn agents_cannot_touch_forbidden_keys() {
        for k in ["agents.permission", "network.listen_port", "advanced.fd_limit", "version", "folders.watch", "general.launch_at_login"] {
            let e = apply(&Config::default(), &changes(json!({ k: 1 }))).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidParams, "{k}");
        }
    }

    #[test]
    fn optional_leaf_can_be_created() {
        // absolute on this system: a drive letter in front on Windows, as is elsewhere
        let arch = if cfg!(windows) { "C:/tmp/arch" } else { "/tmp/arch" };
        let (c, _) = apply(&Config::default(), &changes(json!({"folders.torrent_after_add": "archive", "folders.archive_dir": arch}))).unwrap();
        assert_eq!(c.folders.archive_dir.as_deref(), Some(arch));
    }

    #[test]
    fn all_or_nothing() {
        let before = Config::default();
        let r = apply(&before, &changes(json!({"transfers.download_limit": 1, "transfers.upload_limit": -5})));
        assert!(r.is_err());
    }
}
