//! Settings over the control socket (SPEC 9A.2): an agent with permission full reads and changes every setting the
//! user can change in the Settings window (the user's decision, 2026-11-07: "the CLI should enable the agent to make
//! all changes").
//!
//! - **`config.toml`**: every key of [`Config`], dotted (`transfers.download_limit`, `folders.watch`), except
//!   [`DENIED`]. A value is JSON: a table merges into the table in force (RFC 7396: a key left out keeps its value),
//!   `null` puts a setting back to its default, a list replaces the list. All changes of a call apply together or not
//!   at all, after the type check and [`Config::validate`].
//! - **Keys with a command of their own** ([`ROUTED`]: the development builds, the torrent proxy) are shown but not
//!   set here: their command does more than write the file, and `settings set` names it.
//! - **`rules.toml` and `trackers.toml`** (the Rules tab, the Default rule, favourite trackers): read and replaced
//!   whole, as JSON ([`doc_get`], [`set_rules`], [`set_trackers`]).
//!
//! The running app applies a saved file as it applies its own Settings window's changes: the engine reloads it (the
//! listening port, watch folders, port mapping, limits) and the app applies what is its own (launch at login, keeping
//! the computer awake, the updater).

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use seedwell_state::{Config, Document, RuleSet, TrackerProfiles};

use crate::proto::{ApiError, ErrorCode, clip};

/// Keys out of every agent's reach, read or written, and why (SPEC 9A.3). Everything else in `config.toml` is an
/// agent's to change. A table that holds one of them can still be changed key by key.
pub const DENIED: &[(&str, &str)] = &[
    ("version", "the file's format version, not a setting"),
    ("agents.permission", "an agent must not raise its own access: only the user sets it, in Settings › Agents"),
    ("agents.notify_changes", "the user's notice of what agents change: an agent must not silence it"),
    ("agents.rate_limit_per_sec", "the limit on agents' own calls: an agent must not lift it"),
];

/// Keys (and tables) whose change does more than write `config.toml`: `settings set` refuses them and names the
/// command that does all of it, so a change is never half done. `settings get` shows them. (key, command, why)
pub const ROUTED: &[(&str, &str, &str)] = &[
    (
        "updates.dev_builds",
        "seedwell update dev-builds on|off",
        "switching it off while a development build runs goes back to the latest release",
    ),
    (
        "updates.dev_folder",
        "seedwell update watch <folder> | seedwell update watch --off",
        "the updater starts or stops following the folder",
    ),
    (
        "network.proxy",
        "seedwell proxy on|off | country <code> | nordvpn | custom <host> <port> | remove-key",
        "the app hands the engine the proxy's servers and its key from the secret store; the file alone would hold every torrent or leave the proxy on",
    ),
];

/// The other documents an agent reads and replaces whole (`--file`).
pub const FILES: [&str; 2] = ["rules", "trackers"];

/// `key` is `base` or lies inside it.
pub fn under(key: &str, base: &str) -> bool {
    key == base || key.strip_prefix(base).is_some_and(|r| r.starts_with('.'))
}

/// Why `key` (or something inside it) is out of reach, if it is.
fn denied(key: &str) -> Option<(&'static str, &'static str)> {
    DENIED.iter().copied().find(|(d, _)| under(key, d) || under(d, key))
}

/// The command that changes `key`, if it has one.
fn routed(key: &str) -> Option<(&'static str, &'static str, &'static str)> {
    ROUTED.iter().copied().find(|(r, _, _)| under(key, r) || under(r, key))
}

fn lookup<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(root, |v, part| v.get(part))
}

fn leaves(prefix: &str, v: &Value, out: &mut BTreeMap<String, Value>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                leaves(&key, x, out);
            }
        }
        _ => {
            out.insert(prefix.to_owned(), v.clone());
        }
    }
}

fn json(cfg: &Config) -> Result<Value, ApiError> {
    serde_json::to_value(cfg).map_err(|e| ApiError::internal(format!("settings: {e}")))
}

/// Every setting of `cfg` with its value: dotted key to a JSON value (a list is one value). A setting that is not set
/// (an optional folder) is `null`.
pub fn values(cfg: &Config) -> Result<BTreeMap<String, Value>, ApiError> {
    let mut out = BTreeMap::new();
    leaves("", &json(&sample())?, &mut out);
    for v in out.values_mut() {
        *v = Value::Null;
    }
    leaves("", &json(cfg)?, &mut out);
    Ok(out)
}

/// A config with every optional setting set, so that no key is missing from [`values`] (`config.toml` leaves out what
/// is not set).
fn sample() -> Config {
    use seedwell_state::config::LocationOverride;
    let mut c = Config::default();
    let some = || Some("~/x".to_owned());
    c.folders.archive_dir = some();
    c.advanced.logging.dir = some();
    let both = LocationOverride { incomplete_dir: some(), finished_dir: some() };
    let cats = &mut c.folders.categories;
    for o in [&mut cats.movie, &mut cats.tv, &mut cats.music, &mut cats.audiobook, &mut cats.book, &mut cats.software, &mut cats.other, &mut cats.game] {
        *o = both.clone();
    }
    c
}

fn refused(key: &str, why: &str) -> ApiError {
    ApiError::new(ErrorCode::InvalidParams, format!("setting `{}` is not available to agents: {why}", clip(key, 80)))
        .with_data(serde_json::json!({ "denied": DENIED.iter().map(|(k, _)| *k).collect::<Vec<_>>() }))
}

fn has_command(key: &str, command: &str, why: &str) -> ApiError {
    ApiError::new(ErrorCode::InvalidParams, format!("`{}` has a command of its own: {command} ({why})", clip(key, 80)))
        .with_data(serde_json::json!({ "command": command }))
}

fn unknown(key: &str) -> ApiError {
    ApiError::new(ErrorCode::InvalidParams, format!("unknown setting `{}`; `seedwell settings get` lists every key", clip(key, 80)))
}

/// The requested keys (default: every key an agent may read) with their current values. A key may name a table
/// (`transfers.alt`): its settings come as one JSON object.
pub fn get(cfg: &Config, keys: Option<&[String]>) -> Result<Map<String, Value>, ApiError> {
    let all = values(cfg)?;
    let mut out = Map::new();
    let Some(ks) = keys else {
        for (k, v) in all {
            if denied(&k).is_none() {
                out.insert(k, v);
            }
        }
        return Ok(out);
    };
    let root = json(cfg)?;
    for k in ks.iter().take(100) {
        let inside: Vec<&String> = all.keys().filter(|l| under(l, k)).collect();
        // The key is denied, or a table of denied keys only.
        if let Some((_, why)) = denied(k).filter(|_| inside.iter().all(|l| denied(l).is_some())) {
            return Err(refused(k, why));
        }
        let v = match all.get(k.as_str()) {
            Some(v) => v.clone(),
            // A table: what it holds, without the denied keys.
            None if !inside.is_empty() => {
                let mut v = lookup(&root, k).cloned().unwrap_or_else(|| Value::Object(Map::new()));
                for (d, _) in DENIED {
                    if let Some(rest) = d.strip_prefix(k.as_str()).and_then(|r| r.strip_prefix('.')) {
                        remove(&mut v, rest);
                    }
                }
                v
            }
            None => return Err(unknown(k)),
        };
        out.insert(k.clone(), v);
    }
    Ok(out)
}

fn remove(v: &mut Value, key: &str) {
    match key.split_once('.') {
        None => {
            if let Some(o) = v.as_object_mut() {
                o.remove(key);
            }
        }
        Some((head, rest)) => {
            if let Some(x) = v.get_mut(head) {
                remove(x, rest);
            }
        }
    }
}

/// RFC 7396: a table merges, `null` removes, anything else replaces.
fn merge(target: &mut Value, patch: Value) {
    let Value::Object(p) = patch else {
        *target = patch;
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Some(t) = target.as_object_mut() {
        for (k, v) in p {
            if v.is_null() {
                t.remove(&k);
            } else {
                merge(t.entry(k).or_insert(Value::Null), v);
            }
        }
    }
}

/// `value` at dotted `key`, as a patch of the whole document.
fn patch_at(key: &str, value: Value) -> Value {
    key.rsplit('.').fold(value, |v, part| {
        let mut m = Map::new();
        m.insert(part.to_owned(), v);
        Value::Object(m)
    })
}

/// A list of watch folders as an agent gives it: each entry names its folder by `path`; what it leaves out keeps its
/// value for a folder already watched, or takes a new folder's (Settings' +: on, ask first, leave the `.torrent`
/// file, [`seedwell_state::config::WatchFolder::added`]).
fn complete_watch(current: &Config, list: Value) -> Result<Value, ApiError> {
    let Value::Array(items) = list else { return Ok(list) };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(path) = item.get("path").and_then(Value::as_str) else {
            out.push(item);
            continue;
        };
        let base = match current.folders.watch.iter().find(|w| w.path == path) {
            Some(w) => w.clone(),
            None => seedwell_state::config::WatchFolder::added(path),
        };
        let mut v = serde_json::to_value(base).map_err(|e| ApiError::internal(format!("settings: {e}")))?;
        merge(&mut v, item);
        out.push(v);
    }
    Ok(Value::Array(out))
}

/// A step into a JSON document: a table's key or a list's place.
enum Step {
    Key(String),
    Index(usize),
}

fn path_text(path: &[Step]) -> String {
    let mut s = String::new();
    for step in path {
        match step {
            Step::Key(k) if s.is_empty() => s.push_str(k),
            Step::Key(k) => {
                s.push('.');
                s.push_str(k);
            }
            Step::Index(i) => s.push_str(&format!("[{i}]")),
        }
    }
    s
}

fn at_mut<'a>(v: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    path.iter().try_fold(v, |v, step| match step {
        Step::Key(k) => v.get_mut(k.as_str()),
        Step::Index(i) => v.get_mut(*i),
    })
}

/// The first key of `input` its type ignores (a typo in a list entry, a table or a document), as a dotted path: one
/// that is gone once `input` is parsed and written back (`written`) and that takes a value of any type. A key that is
/// gone only because its value is the default (`skip_serializing_if`) refuses one of the two probes, so it is known.
fn ignored_key(input: &Value, written: &Value, parses: &dyn Fn(&Value) -> bool) -> Option<String> {
    fn walk(root: &Value, here: &Value, written: Option<&Value>, path: &mut Vec<Step>, parses: &dyn Fn(&Value) -> bool) -> Option<String> {
        match here {
            Value::Object(m) => {
                for (k, v) in m {
                    path.push(Step::Key(k.clone()));
                    let w = written.and_then(|w| w.get(k.as_str()));
                    let known = w.is_some()
                        || [Value::String("\u{1}".into()), serde_json::json!({ "\u{1}": 0 })].into_iter().any(|probe| {
                            let mut t = root.clone();
                            at_mut(&mut t, path).is_some_and(|slot| {
                                *slot = probe;
                                true
                            }) && !parses(&t)
                        });
                    if !known {
                        return Some(path_text(path));
                    }
                    if let Some(found) = walk(root, v, w, path, parses) {
                        return Some(found);
                    }
                    path.pop();
                }
                None
            }
            Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    path.push(Step::Index(i));
                    if let Some(found) = walk(root, v, written.and_then(|w| w.get(i)), path, parses) {
                        return Some(found);
                    }
                    path.pop();
                }
                None
            }
            _ => None,
        }
    }
    walk(input, input, Some(written), &mut Vec::new(), parses)
}

/// A key with a dot inside a value (`transfers='{"alt.download_limit":5}'`): tables inside a value are nested.
fn dotted_inside(prefix: &str, v: &Value) -> Option<String> {
    let Value::Object(m) = v else { return None };
    m.iter().find_map(|(k, x)| if k.contains('.') { Some(format!("{prefix}: `{k}`")) } else { dotted_inside(&format!("{prefix}.{k}"), x) })
}

/// Apply `changes` to a copy of `cfg`. Returns the new config (validated) and the keys whose value really changed
/// (none: everything asked was already in force). All-or-nothing: any bad key or value rejects the whole change with
/// field-path errors.
pub fn apply(cfg: &Config, changes: &Map<String, Value>) -> Result<(Config, Vec<String>), ApiError> {
    if changes.is_empty() {
        return Err(ApiError::params("no changes given"));
    }
    if changes.len() > 256 {
        return Err(ApiError::params("more than 256 changes in one call"));
    }
    let known = values(cfg)?;
    let mut root = json(cfg)?;
    let mut problems: Vec<Value> = Vec::new();
    for (k, v) in changes {
        if k.is_empty() || k.split('.').any(str::is_empty) {
            return Err(unknown(k));
        }
        if let Some(d) = dotted_inside(k, v) {
            return Err(ApiError::params(format!("a key inside a value has a dot ({d}): nest the tables, or give the dotted key itself")));
        }
        // The settings this change touches: a table's own keys, else the key itself.
        let mut touched = BTreeMap::new();
        match v {
            Value::Object(_) => leaves(k, v, &mut touched),
            _ => {
                touched.insert(k.clone(), v.clone());
            }
        }
        for t in touched.keys() {
            if let Some((_, why)) = denied(t) {
                return Err(refused(t, why));
            }
            if let Some((_, command, why)) = routed(t) {
                return Err(has_command(t, command, why));
            }
        }
        let mut patch = patch_at(k, v.clone());
        if let Some(list) = patch.get_mut("folders").and_then(|f| f.get_mut("watch")) {
            *list = complete_watch(cfg, list.take())?;
        }
        let mut trial = root.clone();
        merge(&mut trial, patch);
        // Type errors are attributed to the key that caused them.
        let parsed = match serde_json::from_value::<Config>(trial.clone()) {
            Ok(c) => c,
            Err(e) => {
                problems.push(serde_json::json!({ "path": k, "reason": clip(&e.to_string(), 160) }));
                continue;
            }
        };
        // A key the file does not have is dropped by the parse: refused, so a typo never looks like a change, also
        // inside a table or a list entry.
        let after = json(&parsed)?;
        for (t, x) in &touched {
            let there = lookup(&after, t).is_some_and(|a| !a.is_object());
            let is_known = known.contains_key(t.as_str()) || known.keys().any(|l| under(l, t));
            if !(there || (x.is_null() && is_known)) {
                return Err(unknown(t));
            }
        }
        if let Some(bad) = ignored_key(&trial, &after, &|v| serde_json::from_value::<Config>(v.clone()).is_ok()) {
            return Err(unknown(&bad));
        }
        root = trial;
    }
    if !problems.is_empty() {
        // The message names each key and why, as plain output shows only the message.
        let named: Vec<String> = problems.iter().map(|p| format!("{}: {}", p["path"].as_str().unwrap_or(""), p["reason"].as_str().unwrap_or(""))).collect();
        return Err(ApiError::invalid(format!("wrong type: {}", named.join("; "))).with_data(serde_json::json!({ "errors": problems })));
    }
    let mut new: Config = serde_json::from_value(root).map_err(|e| ApiError::invalid(e.to_string()))?;
    new.unknown = cfg.unknown.clone();
    let errs = new.validate();
    if !errs.is_empty() {
        let list: Vec<Value> = errs.iter().map(|e| serde_json::json!({ "path": e.path, "reason": e.reason })).collect();
        let first = errs.first().map(|e| format!("{}: {}", e.path, e.reason)).unwrap_or_default();
        return Err(ApiError::invalid(format!("settings rejected: {first}")).with_data(serde_json::json!({ "errors": list })));
    }
    check_new_folders(folders(cfg), folders(&new))?;
    let now = values(&new)?;
    let changed = known.keys().chain(now.keys()).filter(|k| known.get(*k) != now.get(*k)).cloned().collect::<std::collections::BTreeSet<_>>();
    Ok((new, changed.into_iter().collect()))
}

/// The folders of a location override, under `key`.
fn location_folders<'a>(key: &str, o: &'a seedwell_state::config::LocationOverride, v: &mut Vec<(String, &'a str)>) {
    v.extend(o.incomplete_dir.as_deref().map(|d| (format!("{key}.incomplete_dir"), d)));
    v.extend(o.finished_dir.as_deref().map(|d| (format!("{key}.finished_dir"), d)));
}

/// The folders of every content type, under `key`.
fn category_folders<'a>(key: &str, c: &'a seedwell_state::config::CategoryLocations, v: &mut Vec<(String, &'a str)>) {
    for (name, o) in [("movie", &c.movie), ("tv", &c.tv), ("music", &c.music), ("audiobook", &c.audiobook), ("book", &c.book), ("software", &c.software), ("other", &c.other), ("game", &c.game)] {
        location_folders(&format!("{key}.{name}"), o, v);
    }
}

/// Every folder `config.toml` names, with its key.
fn folders(c: &Config) -> Vec<(String, &str)> {
    let f = &c.folders;
    let mut v = vec![("folders.download_dir".to_owned(), f.download_dir.as_str()), ("folders.incomplete_dir".to_owned(), f.incomplete_dir.as_str())];
    v.extend(f.archive_dir.as_deref().map(|d| ("folders.archive_dir".to_owned(), d)));
    v.extend(c.advanced.logging.dir.as_deref().map(|d| ("advanced.logging.dir".to_owned(), d)));
    category_folders("folders.categories", &f.categories, &mut v);
    for (i, w) in f.watch.iter().enumerate() {
        v.push((format!("folders.watch[{i}].path"), w.path.as_str()));
        v.extend(w.archive_dir.as_deref().map(|d| (format!("folders.watch[{i}].archive_dir"), d)));
    }
    v
}

/// Every folder `rules.toml` names (each rule's finished folder), with its key.
fn rule_folders(r: &RuleSet) -> Vec<(String, &str)> {
    let mut v: Vec<(String, &str)> = r.default.finished_dir.as_deref().map(|d| ("default.finished_dir".to_owned(), d)).into_iter().collect();
    for (i, rule) in r.rules.iter().enumerate() {
        v.extend(rule.finished_dir.as_deref().map(|d| (format!("rule[{i}].finished_dir"), d)));
    }
    v
}

/// Every folder `trackers.toml` names (each favorite's own and per-type folders), with its key.
fn tracker_folders(t: &TrackerProfiles) -> Vec<(String, &str)> {
    let mut v = Vec::new();
    for (i, p) in t.profiles.iter().enumerate() {
        location_folders(&format!("tracker[{i}].location"), &p.location, &mut v);
        category_folders(&format!("tracker[{i}].categories"), &p.categories, &mut v);
    }
    v
}

/// A folder an agent names is never one where a file could become code or configuration (`~/Library`, a hidden
/// folder of the home folder, a system folder): the rule `add --save-path` and `move` follow (SPEC 9A.3). Folders
/// already in force (`before`) stay as the user chose them.
fn check_new_folders(before: Vec<(String, &str)>, after: Vec<(String, &str)>) -> Result<(), ApiError> {
    let before: Vec<&str> = before.into_iter().map(|(_, d)| d).collect();
    for (key, d) in after {
        if before.contains(&d) {
            continue;
        }
        let path = crate::ops::clean_path(d, &key)?;
        if crate::ops::is_sensitive_dir(&path) {
            return Err(ApiError::invalid(format!("`{key}` points at a system, Library or hidden folder ({}); choose a normal folder", clip(d, 200))));
        }
    }
    Ok(())
}

/// Which document `file` names (`rules`, `trackers`).
pub fn check_file(file: &str) -> Result<&'static str, ApiError> {
    FILES.iter().copied().find(|f| *f == file).ok_or_else(|| ApiError::params(format!("--file is rules or trackers, not `{}`", clip(file, 40))))
}

/// `rules.toml` or `trackers.toml` as JSON (the shape [`set_rules`] and [`set_trackers`] take back).
pub fn doc_get(rules: &RuleSet, trackers: &TrackerProfiles, file: &str) -> Result<Value, ApiError> {
    let v = match check_file(file)? {
        "rules" => serde_json::to_value(rules),
        _ => serde_json::to_value(trackers),
    };
    v.map_err(|e| ApiError::internal(format!("settings: {e}")))
}

/// A whole document from JSON, strictly: a key its type does not have is refused by name, never dropped.
fn doc_from<T: Document + serde::Serialize + serde::de::DeserializeOwned>(doc: Value) -> Result<T, ApiError> {
    let bad = |e: &dyn std::fmt::Display| ApiError::invalid(format!("{}: {}", T::FILE, clip(&e.to_string(), 300)));
    let new: T = serde_json::from_value(doc.clone()).map_err(|e| bad(&e))?;
    let written = serde_json::to_value(&new).map_err(|e| ApiError::internal(format!("settings: {e}")))?;
    if let Some(key) = ignored_key(&doc, &written, &|v| serde_json::from_value::<T>(v.clone()).is_ok()) {
        return Err(ApiError::new(ErrorCode::InvalidParams, format!("{}: unknown key `{}`", T::FILE, clip(&key, 120))));
    }
    Ok(new)
}

/// A whole new `rules.toml` from JSON, in place of `old`: strictly read, validated as the file is, its folders held to
/// the rule of [`apply`]; the keys this build does not know (JSON never carries them) stay from `old`. Nothing is saved.
pub fn set_rules(old: &RuleSet, doc: Value) -> Result<RuleSet, ApiError> {
    let mut new: RuleSet = doc_from(doc)?;
    new.unknown = old.unknown.clone();
    new.render().map_err(|e| ApiError::invalid(format!("{}: {}", RuleSet::FILE, clip(&e.to_string(), 300))))?;
    check_new_folders(rule_folders(old), rule_folders(&new))?;
    Ok(new)
}

/// A whole new `trackers.toml` from JSON, in place of `old`, as [`set_rules`].
pub fn set_trackers(old: &TrackerProfiles, doc: Value) -> Result<TrackerProfiles, ApiError> {
    let mut new: TrackerProfiles = doc_from(doc)?;
    new.unknown = old.unknown.clone();
    new.render().map_err(|e| ApiError::invalid(format!("{}: {}", TrackerProfiles::FILE, clip(&e.to_string(), 300))))?;
    check_new_folders(tracker_folders(old), tracker_folders(&new))?;
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn changes(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    /// Failure: a value of the wrong type refused with "some values have the wrong type" and no key: the CLI's plain
    /// output shows only the message, so nobody could tell which value (PC report on win-100).
    #[test]
    fn a_wrong_type_names_its_key() {
        let e = apply(&Config::default(), &changes(json!({"transfers.alt": "{schedule:{enabled:true}}", "transfers.max_active_downloads": 3}))).unwrap_err();
        assert!(e.message.starts_with("wrong type: transfers.alt: "), "{}", e.message);
        assert_eq!(e.data.as_ref().unwrap()["errors"][0]["path"], json!("transfers.alt"));
    }

    #[test]
    fn private_min_seed_is_a_setting_with_a_ten_day_default() {
        let all = get(&Config::default(), None).unwrap();
        assert_eq!(all["transfers.private_min_seed_secs"], json!(864_000));
        let (c, ch) = apply(&Config::default(), &changes(json!({"transfers.private_min_seed_secs": 0}))).unwrap();
        assert_eq!((c.transfers.private_min_seed_secs, ch.len()), (0, 1));
    }

    /// Failure: a setting of `config.toml` that an agent cannot reach (the old allow-list left out the network, the
    /// watch folders, launch at login, most of Advanced), or a denied one that it can read.
    #[test]
    fn every_key_but_the_denied_ones_is_listed_and_settable() {
        let cfg = Config::default();
        let all = get(&cfg, None).unwrap();
        for k in ["network.listen_port", "network.dht", "network.upnp", "folders.watch", "general.launch_at_login", "advanced.fd_limit", "advanced.logging.level", "folders.categories.movie.finished_dir", "folders.archive_dir", "updates.dev_builds", "network.proxy.enabled"] {
            assert!(all.contains_key(k), "{k} is listed");
        }
        assert!(!all.keys().any(|k| denied(k).is_some()), "{:?}", all.keys().filter(|k| denied(k).is_some()).collect::<Vec<_>>());
        // Each listed key that has no command of its own takes its own value back: nothing changes.
        let settable: Map<String, Value> = all.into_iter().filter(|(k, v)| routed(k).is_none() && !v.is_null()).collect();
        let (c, ch) = apply(&cfg, &settable).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!((c, ch), (cfg, Vec::<String>::new()));
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

    /// Failure: an agent raises its own access, silences the notice of its changes, lifts its rate limit or rewrites
    /// the format version, by the key itself or through the table that holds it.
    #[test]
    fn the_denied_keys_stay_denied_by_key_and_by_table() {
        for (k, v) in [
            ("agents.permission", json!("full")),
            ("agents", json!({"permission": "full"})),
            ("agents", json!({"notify_changes": false})),
            ("agents.notify_changes", json!(false)),
            ("agents.rate_limit_per_sec", json!(1000)),
            ("version", json!(1)),
        ] {
            let e = apply(&Config::default(), &changes(json!({ k: v }))).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidParams, "{k}");
            assert!(e.message.contains("not available to agents"), "{}", e.message);
        }
        for k in ["agents.permission", "agents", "version"] {
            assert!(get(&Config::default(), Some(&[k.into()])).is_err(), "{k}");
        }
        assert!(!get(&Config::default(), None).unwrap().keys().any(|k| k.starts_with("agents") || k == "version"));
    }

    /// Failure: `settings set` writes a key whose change needs more than the file (the proxy's servers and key, the
    /// updater following a folder), leaving the file and the running app disagreeing.
    #[test]
    fn keys_with_a_command_of_their_own_are_refused_with_the_command() {
        for (k, v, cmd) in [
            ("updates.dev_builds", json!(true), "update dev-builds"),
            ("updates.dev_folder", json!("/b"), "update watch"),
            ("network.proxy.enabled", json!(true), "seedwell proxy"),
            ("network.proxy", json!({"country": "NL"}), "seedwell proxy"),
            ("network", json!({"proxy": {"enabled": false}}), "seedwell proxy"),
        ] {
            let e = apply(&Config::default(), &changes(json!({ k: v }))).unwrap_err();
            assert!(e.code == ErrorCode::InvalidParams && e.message.contains(cmd), "{k}: {}", e.message);
        }
        // The rest of the table is the agent's.
        let (c, _) = apply(&Config::default(), &changes(json!({"network": {"dht": false, "listen_port": 51515}}))).unwrap();
        assert!(!c.network.dht && c.network.listen_port == 51515);
        assert!(get(&Config::default(), Some(&["network.proxy".into()])).is_ok(), "shown");
    }

    #[test]
    fn an_agent_can_turn_on_install_automatically_so_a_device_follows_the_others_to_a_release() {
        let (c, ch) = apply(&Config::default(), &changes(json!({"updates.auto_install": true}))).unwrap();
        assert!(c.updates.check && c.updates.auto_install, "{ch:?}");
        assert_eq!(get(&c, Some(&["updates.auto_install".into()])).unwrap()["updates.auto_install"], json!(true));
    }

    /// Failure: a typo (`transfers.downlod_limit`) is dropped by the parse and reported as changed.
    #[test]
    fn an_unknown_key_is_refused() {
        for k in ["transfers.downlod_limit", "nope", "transfers.alt.x", "transfers..download_limit", ""] {
            let e = apply(&Config::default(), &changes(json!({ k: 1 }))).unwrap_err();
            assert!(e.code == ErrorCode::InvalidParams && e.message.contains("unknown setting"), "{k}: {e:?}");
        }
        assert!(apply(&Config::default(), &changes(json!({"transfers.alt": {"enabeld": true}}))).is_err());
        assert!(get(&Config::default(), Some(&["transfers.nope".into()])).is_err());
    }

    #[test]
    fn optional_leaf_can_be_created_and_cleared_with_null() {
        // absolute on this system: a drive letter in front on Windows, as is elsewhere
        let arch = if cfg!(windows) { "C:/tmp/arch" } else { "/tmp/arch" };
        let (c, _) = apply(&Config::default(), &changes(json!({"folders.torrent_after_add": "archive", "folders.archive_dir": arch}))).unwrap();
        assert_eq!(c.folders.archive_dir.as_deref(), Some(arch));
        let (c, _) = apply(&c, &changes(json!({"folders.categories.movie.finished_dir": "~/Movies"}))).unwrap();
        assert_eq!(c.folders.categories.movie.finished_dir.as_deref(), Some("~/Movies"));
        let (c, _) = apply(&c, &changes(json!({"folders.categories.movie.finished_dir": null, "folders.torrent_after_add": "leave", "folders.archive_dir": null}))).unwrap();
        assert!(c.folders.categories.movie.finished_dir.is_none() && c.folders.archive_dir.is_none());
    }

    /// Failure: a table given as JSON resets what it leaves out (the alternative limits lost when only the schedule
    /// was meant), or `null` leaves a value the user cannot get back to.
    #[test]
    fn a_table_merges_and_null_is_the_default() {
        let mut cfg = Config::default();
        cfg.transfers.alt.download_limit = 123;
        let (c, _) = apply(&cfg, &changes(json!({"transfers.alt": {"schedule": {"enabled": true, "from": "08:00"}}}))).unwrap();
        assert_eq!((c.transfers.alt.download_limit, c.transfers.alt.schedule.enabled, c.transfers.alt.schedule.from.0), (123, true, 480));
        let (c, _) = apply(&c, &changes(json!({"transfers.alt.download_limit": null}))).unwrap();
        assert_eq!(c.transfers.alt.download_limit, Config::default().transfers.alt.download_limit);
        assert_eq!(get(&c, Some(&["transfers.alt.schedule".into()])).unwrap()["transfers.alt.schedule"]["from"], json!("08:00"));
    }

    /// Failure: a watch folder an agent adds with only its path takes the file's defaults (add without asking, the
    /// `.torrent` file to the Trash) instead of the ones Settings' + gives, or an edit of one folder resets another's
    /// fields (its `since`, so the backlog is offered again).
    #[test]
    fn a_watch_folder_added_by_path_is_the_one_settings_adds() {
        let (c, _) = apply(&Config::default(), &changes(json!({"folders.watch": [{"path": "~/Torrents"}]}))).unwrap();
        assert_eq!(c.folders.watch, vec![seedwell_state::config::WatchFolder::added("~/Torrents")]);
        let mut cfg = c;
        cfg.folders.watch[0].since = Some(1_700_000_000);
        let (c, _) = apply(&cfg, &changes(json!({"folders.watch": [{"path": "~/Torrents", "mode": "auto"}, {"path": "~/More"}]}))).unwrap();
        assert_eq!((c.folders.watch[0].since, c.folders.watch.len()), (Some(1_700_000_000), 2));
        assert_eq!(c.folders.watch[0].mode, seedwell_state::config::WatchMode::Auto);
        let e = apply(&cfg, &changes(json!({"folders.watch": [{"path": "~/A"}, {"path": "~/A"}]}))).unwrap_err();
        assert!(e.message.contains("duplicate"), "{}", e.message);
        let (c, _) = apply(&cfg, &changes(json!({"folders.watch": []}))).unwrap();
        assert!(c.folders.watch.is_empty());
    }

    /// Failure: an agent (or a message that talks one into it) points downloads, a watch folder or the logs at a
    /// folder where a file becomes code or configuration (`~/Library/LaunchAgents`, `~/.ssh`), which `add --save-path`
    /// already refuses.
    #[test]
    fn an_agent_cannot_point_a_folder_at_a_system_library_or_hidden_folder() {
        // The user's settings folder of this system.
        let settings = if cfg!(windows) { "~/AppData/Roaming/Startup" } else { "~/Library/LaunchAgents" };
        for (k, v) in [
            ("folders.download_dir", json!(settings)),
            ("folders.incomplete_dir", json!("~/.ssh")),
            ("folders.categories.software.finished_dir", json!("~/.config/autostart")),
            ("folders.watch", json!([{"path": "~/.hidden"}])),
            ("advanced.logging.dir", json!("~")),
        ] {
            let e = apply(&Config::default(), &changes(json!({ k: v }))).unwrap_err();
            assert!(e.message.contains("system, Library or hidden folder"), "{k}: {}", e.message);
        }
        assert!(apply(&Config::default(), &changes(json!({"folders.download_dir": "~/Media/Done"}))).is_ok());
    }

    /// Failure: a typo inside a list entry, a table or a rules or trackers document is dropped by the parse, so the
    /// change is reported as made while the setting keeps its old or default value (a watch folder added in Ask mode
    /// when "mode" was misspelt).
    #[test]
    fn a_typo_inside_a_value_or_a_document_is_refused_by_name() {
        for (k, v, bad) in [
            ("folders.watch", json!([{"path": "~/T", "mdoe": "auto"}]), "folders.watch[0].mdoe"),
            ("transfers.alt", json!({"schedule": {"enabeld": true}}), "transfers.alt.schedule.enabeld"),
            ("folders.categories", json!({"movie": {"finshed_dir": "~/M"}}), "folders.categories.movie.finshed_dir"),
        ] {
            let e = apply(&Config::default(), &changes(json!({ k: v }))).unwrap_err();
            assert!(e.message.contains(bad), "{k}: {}", e.message);
        }
        // What is left out because it is the default (`skip_serializing_if`) is no typo.
        assert!(apply(&Config::default(), &changes(json!({"folders.watch": [{"path": "~/T", "archive_dir": null, "category": null}]}))).is_ok());
        let rules = RuleSet::default();
        let mut doc = doc_get(&rules, &TrackerProfiles::default(), "rules").unwrap();
        doc["default"]["on_gaol"] = json!("stop");
        let e = set_rules(&rules, doc).unwrap_err();
        assert!(e.message.contains("default.on_gaol"), "{}", e.message);
        let e = set_trackers(&TrackerProfiles::default(), json!({"version": 1, "tracker": [{"id": "tl", "name": "TL", "urls": ["tl.example"], "hiden": true}]})).unwrap_err();
        assert!(e.message.contains("tracker[0].hiden"), "{}", e.message);
        // A favorite that sets only what differs from the defaults is read whole.
        let t = set_trackers(&TrackerProfiles::default(), json!({"version": 1, "tracker": [{"id": "tl", "name": "TL", "urls": ["tl.example"], "hidden": false}]})).unwrap();
        assert_eq!(t.profiles.len(), 1);
    }

    /// Failure: a dotted key inside a table value, or an empty table, changes nothing and is reported (and logged) as
    /// a change.
    #[test]
    fn only_what_really_changed_is_reported() {
        let e = apply(&Config::default(), &changes(json!({"transfers": {"alt.download_limit": 5}}))).unwrap_err();
        assert!(e.message.contains("alt.download_limit"), "{}", e.message);
        for nothing in [json!({"transfers": {}}), json!({"transfers.max_active_downloads": Config::default().transfers.max_active_downloads})] {
            let (c, ch) = apply(&Config::default(), &changes(nothing.clone())).unwrap();
            assert_eq!((c, ch), (Config::default(), Vec::<String>::new()), "{nothing}");
        }
        let (_, ch) = apply(&Config::default(), &changes(json!({"transfers": {"alt": {"download_limit": 5}, "max_active_downloads": 3}}))).unwrap();
        assert_eq!(ch, ["transfers.alt.download_limit"]);
    }

    /// Failure: a rule's finished folder or a favorite tracker's folders, written through `--file`, point at a folder
    /// where a file becomes code or configuration, which `settings set` and `add --save-path` refuse.
    #[test]
    fn the_folder_rule_holds_for_rules_and_trackers_too() {
        let settings = if cfg!(windows) { "~/AppData/Roaming/Startup" } else { "~/Library/LaunchAgents" };
        let rules = RuleSet::default();
        let mut doc = doc_get(&rules, &TrackerProfiles::default(), "rules").unwrap();
        doc["default"]["finished_dir"] = json!(settings);
        let e = set_rules(&rules, doc).unwrap_err();
        assert!(e.message.contains("default.finished_dir") && e.message.contains("hidden folder"), "{}", e.message);
        let mut rule = doc_get(&rules, &TrackerProfiles::default(), "rules").unwrap()["default"].clone();
        rule["id"] = json!("x");
        rule["name"] = json!("X");
        rule["finished_dir"] = json!("~/.config/x");
        let mut doc = doc_get(&rules, &TrackerProfiles::default(), "rules").unwrap();
        doc["rule"] = json!([rule]);
        assert!(set_rules(&rules, doc).unwrap_err().message.contains("rule[0].finished_dir"));
        let fav = |loc: Value| json!({"version": 1, "tracker": [{"id": "tl", "name": "TL", "urls": ["tl.example"], "location": loc}]});
        let e = set_trackers(&TrackerProfiles::default(), fav(json!({"finished_dir": settings}))).unwrap_err();
        assert!(e.message.contains("tracker[0].location.finished_dir"), "{}", e.message);
        let mut t = fav(json!({}));
        t["tracker"][0]["categories"] = json!({"movie": {"incomplete_dir": "~/.cache/x"}});
        assert!(set_trackers(&TrackerProfiles::default(), t).unwrap_err().message.contains("tracker[0].categories.movie.incomplete_dir"));
        // A folder already in the file stays as the user chose it.
        let mut old = TrackerProfiles::default();
        let kept = set_trackers(&old, fav(json!({"finished_dir": "~/Media"}))).unwrap();
        old = kept;
        old.profiles[0].location.finished_dir = Some(settings.into());
        assert!(set_trackers(&old, doc_get(&rules, &old, "trackers").unwrap()).is_ok());
    }

    #[test]
    fn all_or_nothing() {
        let before = Config::default();
        let r = apply(&before, &changes(json!({"transfers.download_limit": 1, "transfers.upload_limit": -5})));
        assert!(r.is_err());
        let r = apply(&before, &changes(json!({"transfers.download_limit": 1, "agents.permission": "full"})));
        assert!(r.is_err());
    }

    /// Failure: a rules or trackers document that does not validate is saved, or keys this build does not know are
    /// lost when an agent writes the document back.
    #[test]
    fn rules_and_trackers_round_trip_and_are_validated() {
        let rules = RuleSet::default();
        let trackers = TrackerProfiles::default();
        let doc = doc_get(&rules, &trackers, "rules").unwrap();
        let unknown = RuleSet::parse_str("version = 1\nfuture = true\n").unwrap().value.unknown;
        assert!(!unknown.is_empty());
        let old = RuleSet { unknown: unknown.clone(), ..RuleSet::default() };
        let back = set_rules(&old, doc.clone()).unwrap();
        assert_eq!((back.default.clone(), back.unknown.clone()), (rules.default.clone(), unknown));
        let mut bad = doc;
        bad["version"] = json!(999);
        assert!(set_rules(&rules, bad).is_err());
        let t = set_trackers(&trackers, doc_get(&rules, &trackers, "trackers").unwrap()).unwrap();
        assert_eq!(t.profiles, trackers.profiles);
        assert!(check_file("config").is_err());
    }
}
