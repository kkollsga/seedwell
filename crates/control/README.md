# seedwell-control

The agent interface of Seedwell (SPEC 9A): a **versioned JSON-lines control protocol**, the **local
server** behind `control.sock`, and a small **async client** used by the `seedwell` CLI
and the tests. `#![forbid(unsafe_code)]`.

```
seedwell CLI ─┐
seedwell sim ─┼─ JSON lines over a Unix socket ─▶ seedwell-control server ─▶ Backend ─▶ seedwell_engine::EngineHandle
Claude, tools ┘   (~/Library/Application Support/Seedwell/control.sock)     └─────▶ seedwell-state (stats, rules, profiles, settings)
```

| Module | Role |
|---|---|
| `proto` | Wire format, the `methods!` table (every method, params type, required permission), the request decoder (fuzzed) |
| `types` | Result DTOs (serde); new fields are `Option`/`#[serde(default)]` so old clients keep working |
| `ops` | Pure logic: id resolution (full id or unique prefix), list filters, input validation for `add` |
| `server` | `serve_backend`: accept loop, peer check, per-connection handler, permission gate, rate limit, activity log, events, two-step remove |
| `backend` | `trait Backend`, what the server needs from the engine (also what tests mock) |
| `engine_backend` (feature `engine`) | `EngineBackend`: the thin mapping from `EngineHandle`; `serve(..)` and `start_for_app(..)` |
| `socket` | Where the socket lives, atomic creation with mode 0600 in a 0700 directory, `peer_allowed` |
| `confirm` | Two-step remove tokens (60 s, single use, per client name) |
| `perm`, `activity`, `limits`, `redact`, `settings`, `state`, `human`, `b64`, `client` | Permission sources, activity log, line reader and rate limiter, secret scrubbing, settings (every key but `DENIED`; `ROUTED` keys name their command; rules and trackers documents), state access, formatting, base64, client |
| `builds`, `proxy` | Where the server finds the app's updater (`update.*`, `restart`) and torrent proxy (`proxy.*`); a daemon has neither |

## Starting the server

```rust
// Inside the app (seedwell-ffi), after the engine started on runtime `rt`:
let handle = seedwell_control::start_for_app(&rt, engine.handle(), seedwell_control::AppServerConfig {
    socket_path: None,                                   // default: ~/Library/Application Support/Seedwell/control.sock
    state_dir,                                           // config.toml, rules.toml, trackers.toml
    store: Some(host.store().clone()),                   // Arc<StateStore> for statistics (it implements StoreRef)
    permission: PermissionSource::from_config_dir(&state_dir), // or ::shared(..) and call .set(..) from Settings
    options: ServerOptions::default(),
    tz_offset: None,                                     // None = UTC; pass the app's offset for local "today"
})?;
handle.activity().set_sink(|e| { /* "Claude paused 3 downloads" notification, Settings -> Agents log */ });
// keep `handle` alive for the life of the app; dropping it closes the socket
```

`start_for_app` must be called from a thread that is not inside the runtime (it uses `Handle::block_on`);
use `serve(engine, state, path, permission).await` from async code. Headless: `seedwell daemon`.

## Security model

* **Unix domain socket only. No TCP listener, ever.** The socket is created inside a private 0700
  directory next to its final path, chmodded 0600, and *then* renamed into place, so there is no
  moment at which another user could connect. The parent directory is created 0700 if missing; an
  existing parent with group/other access is reported (`ServerHandle::warnings`) but never
  chmodded (it could be `$HOME`). A stale socket from a crashed run is replaced; a live one makes
  start fail with `AddrInUse`.
* **Same uid only.** Every accepted connection is checked with `UnixStream::peer_cred()` against the
  uid that owns the socket file (`socket::peer_allowed`); anyone else is dropped without an answer.
  (This is the `getpeereid` check, through tokio, without `unsafe`.)
* **Permission levels** (`agents.permission` in `config.toml`, or any `PermissionSource`), read on
  *every call*, so Settings changes apply to open connections: `off` refuses the connection
  (`agent_access_off`, then close); `read_only` allows queries; `control` adds the mutating methods
  except file deletion and settings; `full` adds `remove` with `delete_files`, `set_seed_goal` with
  `at_goal: remove_and_delete`, `settings_get`, `settings_set` and `activity`. Permission is checked **before** the params
  are parsed.
* **Two-step remove.** `remove` returns a plan (names, bytes on disk, paths, whether files will be
  deleted) and a 128-bit token, stored for 60 s. `remove_confirm` redeems it once, for the same
  client name. File deletion needs `full` at step 1 and again at step 2 (the level may have dropped).
* **Limits.** At most 8 connections (the 9th gets `too_many_connections`); request lines are capped
  (4 KiB before `hello`, 24 MiB after, which holds a base64 16 MiB `.torrent`); 20 requests/s per
  connection (burst 40) with `rate_limited` + `retry_after_ms`; eight unparseable lines in a row close
  the connection; a client that stops reading is dropped after 10 s; ids per call <= 1000.
* **Activity log.** Every mutating call, allowed or denied, is recorded with the `hello` client name,
  a redacted description and the outcome; `ServerHandle::activity()` for the app,
  `activity` over the protocol (`full`).
* **Nothing secret goes out.** Announce URLs lose passkeys (`seedwell_state::url::redact_url`), URLs and
  error texts are scrubbed of anything URL-shaped, magnet links in names/logs keep only `xt` and `dn`,
  direct-download URLs lose their query. Peer addresses appear only in `info`. There are no client-identity
  or compliance settings to expose. `settings_get`/`settings_set` reach every key of `config.toml` except
  `settings::DENIED` (`agents.permission`, `agents.notify_changes`, `agents.rate_limit_per_sec`, `version`: an agent
  cannot raise its own access, silence the notice of its changes or lift its own limit); `settings::ROUTED` keys
  (`updates.dev_builds`, `updates.dev_folder`, `network.proxy.*`) are refused with the command that does all of
  their change. The proxy's key goes in through `proxy.key` only and never comes back. Tracker
  timing is limited to coarse last/next announce times; `reannounce` goes through the engine's tracker
  gate, which enforces the minimum interval, and the answer carries `not_before`.

## Protocol reference (version 1)

One JSON object per line, UTF-8, `\n`-terminated.

```
request   {"v":1,"id":<number|string>,"method":"<name>","params":{...}}          ("v" optional, must be 1; params optional = {})
success   {"v":1,"id":<same>,"result":{...}}
failure   {"v":1,"id":<same or null>,"error":{"code":"<code>","message":"...","data":{...}}}
event     {"v":1,"event":"<kind>","seq":<n>,"data":{"seq":..,"at":<unix ms>,"kind":..,"id":..,"name":..,"data":..}}   (only after events_subscribe)
```

The first request on a connection must be `hello`. Responses come in request order; events may
arrive between a request and its response. Unknown request fields are rejected with
`invalid_params` (typos are errors), unknown *response* fields must be ignored by clients.
Ids in params are the 40-hex transfer id or any unique prefix of at least 6 hex characters.

### Methods

| Method | Min. level | Logged | Params | Result |
|---|---|---|---|---|
| `hello` | read_only | no | `client` (string, 1-64 chars), `protocol?`, `version?` | `protocol`, `server`, `permission`, `methods` (allowed now), `rate_limit_per_sec`, `pid` (the server's process), `started_ms` (when it started answering: a restarted app answers with another `pid` or a later one) |
| `ping` | read_only | no | - | `{pong, time_ms}` |
| `status` | read_only | no | - | `server`, `protocol`, `permission`, `totals` (rates, connections, port, tiers), `counts` by state, `total`, `limits`, `summary` |
| `list` | read_only | no | `filter?` (`all` `downloading` `queued` `seeding` `low-power` `paused` `error` `finished` `recent` `checking` `metadata` `awaiting-choice`), `tracker?` (profile id, `public`, `other-private` or host part), `category?`, `text?`, `kind?` (`torrent`/`direct`), `limit?` (50, max 500), `offset?` | `total`, `offset`, `transfers[]` (every field of a row, plus `human` strings) |
| `info` | read_only | no | `id`, `files?`, `trackers?`, `peers?` (default true), `max_files?` (2000), `max_peers?` (100) | `transfer`, `files[]` (`index`, `path`, `length`, `done`, `priority`), `trackers[]` (redacted URL, tier, status, seeds, leechers, last/next announce as Unix s), `peers[]`, `*_truncated`, `piece_count`, `piece_length`, `comment` |
| `trackers` | read_only | no | - | `profiles[]` (`id`, `name`, `urls`, `rule`, `private`, `allow_pex`, live `transfers`/`sharing`/rates), `groups[]` (automatic trackers, same fields), `public_transfers`, `other_private_transfers`, `other_private_allow_pex`, `pex_enabled` |
| `tracker_stats` | read_only | no | `tracker?` (profile id, `public`, `other-private`, `all`), `days?` (30, max 400) | `today`, `this_week`, `this_month`, `all_time` (bytes up/down, ratio, completed...), daily `series[]`, `summary` |
| `rules` | read_only | no | - | the contents of `rules.toml` (`default`, `rule[]`) |
| `settings_get` | full | no | `keys?` (dotted keys or tables; absent: every key but the denied ones, plus `network.proxy.key`: installed/none with the app's proxy), or `file` (`rules`, `trackers`) | `settings` map (with `file`: `{<file>: the document as JSON}`) |
| `events_subscribe` | read_only | no | `kinds?`, `hold?` (a name this connection holds alone while open: `duplicate` while another holds it) | `{subscribed, cursor}`, then pushed events |
| `events_unsubscribe` | read_only | no | - | `{subscribed:false}` |
| `events` | read_only | no | `since?` (cursor), `limit?` (100, max 500), `kinds?` | `events[]`, `cursor`, `oldest_available` (ring of the last 1024 events) |
| `wait_for` | read_only | no | `ids`, `mode?` (`all`/`any`), `timeout_secs?` (60, max 300) | `done`, `timed_out`, `transfers[]`. Blocks this connection until done |
| `add` | control | yes | exactly one of `source` (auto: magnet / http(s) URL / path), `magnet`, `torrent_path`, `torrent_base64`, `url`; `save_path?` (needs `full`; never `~/Library`, hidden home folders or system paths), `paused?`, `category?`, `priority?`, `rule?`, `queue_top?`, `sha256?` (direct), `peers?` (`ip:port`, advanced), `now?` (start at once: without it a torrent that would start without asking is first held for `transfers.start_delay_secs`, 10 s by default, SPEC 7A.7) | `id`, `name`, `kind`, and when the torrent is held for a choice (smart add, SPEC 7A.6): `needs_choice: true` + `choice` (the `choice` result). A magnet is held once its metadata is in: watch for the `choice_needed` event, then call `choice`. While the torrent waits out the grace period the answer has `pending: {starts_at_ms, starts_in_secs, paused}`, the row has state `pending` with the same `pending` object, `list pending` filters them and a `pending` event (`data.change`: added, paused, resumed, started, chosen) follows it |
| `pause` / `resume` | control | yes | `ids` or `all: true` | `action`, `count`, `transfers[]` |
| `move_to_top` | control | yes | `ids` | same |
| `set_priority` | control | yes | `ids`, `priority` (`high` `normal` `low`) | same |
| `set_file_priorities` | control | yes | `id`, and either `priorities` (one per file) or `set: [{index, priority}]` (`skip` `low` `normal` `high`) | same |
| `set_category` | control | yes | `ids`, `category` (7 names, or null to detect again) | same |
| `set_seed_goal` | control (`remove_and_delete`: full) | yes | `ids`, `ratio?`, `seed_secs?`, `min_seed_secs?`, `any?`, `at_goal?` (`stop` `remove` `remove_and_delete`), `stop_on_finish?`, `forever?` | same |
| `set_tracker_limits` | control | yes | `id`, `down_limit?`, `up_limit?` (bytes/s, 0 = unlimited; favorites only; one cap for all the tracker's torrents together) | `id`, `down_limit`, `up_limit`; written to `trackers.toml` |
| `set_tracker_pex` | full | yes | `id` (a favorite's id, an automatic tracker's domain or `other-private`), `allow` (bool) | `id`, `allow_pex`; written to `trackers.toml`. Peer exchange for a private tracker's torrents, off by default; `network.pex` (master switch) overrides it |
| `set_limits` | control | yes | `down_limit?`, `up_limit?`, `alt_down_limit?`, `alt_up_limit?` (bytes/s, 0 = unlimited), `alt_speed?` | `limits` in effect; also written to `config.toml` |
| `reannounce` | control | yes | `ids`, `force` (also past the tracker's minimum interval, once per tracker per 5 min), `wait` (up to 30 s for the answer) | per transfer: trackers with `url` (scheme and host only), `status` (sent / waiting: Seedwell is backing off after N failed tries / deferred: the tracker's minimum interval / forced limit), `not_before` (Unix s), `answer` (with `wait`), `note` |
| `recheck` | control | yes | `ids` | action |
| `remove` | control (`delete_files`: full) | yes | `ids`, `delete_files?` | `token`, `expires_in_secs`, `delete_files`, `items[]`, `summary`. **Removes nothing.** |
| `remove_confirm` | control (full if the plan deletes files) | yes | `token` | `removed[]`, `missing[]`, `files_deleted` |
| `settings_set` | full | yes | `changes` (dotted key to value: a table merges, `null` is the default, a list replaces; all-or-nothing, validated by `seedwell-state`), or `file` (`rules`, `trackers`) with `doc` (the whole document, validated as the file is) | `changed[]`, `note` |
| `tracker_log_clear` | full | yes | - | `cleared`: the developer-mode tracker log is deleted (Settings › Advanced › Developer › Clear Log) |
| `proxy.status` | full | no | - | `enabled`, `kind`, `host`, `port`, `country`, `has_key` (never the key), `server`, `status`, `problem` |
| `proxy.set` | full | yes | `on?`, `country?` (ISO code, "" none) | as `proxy.status`, with `note` |
| `proxy.key` | full | yes | `kind` (`nordvpn`: `user`, `pass`; `custom`: `host`, `port`, both or neither of `user`, `pass`); tested before it is kept, never logged or returned | as `proxy.status`, with `note` |
| `proxy.remove_key` / `proxy.faster` | full | yes | - | as `proxy.status`, with `note` |
| `activity` | full | no | `limit?` (50) | `entries[]` (`at`, `client`, `method`, `summary`, `outcome`) |
| `update.status` | read_only | no | - | `version`, `development`, `dev_builds`, `watching`, `kept`, `status` (the app's updater, SPEC 12A; a daemon has none: `unsupported`) |
| `update.install` / `update.watch` | control | yes | `path`, `sha256?` / `folder?` (absent: stop) | as `update.status`, with `note`; need "Allow development builds" on |
| `update.dev_builds` / `update.now` | full | yes | `on` / - | as `update.status`, with `note` |
| `restart` | full | yes | - | `note` (what happens and what it interrupts), `log`, `pid` and `started_ms` of the run that quits. The app's own installer quits this process the proper way and opens the app again (SPEC 9A.10); the answer comes first. Refused (`invalid`) while an install or another restart runs; a daemon: `unsupported` |
| `awake.set` | control | no | `key` (`agent:<name>`; taking it again replaces the time), `who?` (what is working, shown), `secs?` (at most a day; absent or 0 gives the lease back) | as `awake.status`. Keeps the Mac from idle sleep while the lease is live (SPEC 9A.7); not in the activity log |
| `awake.status` | read_only | no | - | `leases[]` (`key`, `who`, `client`, `since_ms`, `until_ms`), `setting` (`general.keep_awake_for_agents`), `host_keeps` (the app holds a power assertion; a headless daemon cannot), `kept` |
| `tracker_log` | read_only | no | `limit?` (50, max 500), `torrent?` (info-hash prefix or title word), `tracker?` (host part), `event?`, `errors_only?`, `since_ms?` (follow) | `enabled` (Settings > Advanced > Developer mode), `path`, `entries[]`: the developer-mode tracker log lines, newest last (SPEC 8.7). Passkeys and the client `key` are masked in the file itself. Empty while developer mode is off |
| `choice` | read_only | no | `id` | The chooser of a held (state `awaiting-choice`) or running torrent: `reasons[]` (`kind`, `text`), `structure` (`tv` `movies` `music` `books` `audiobooks` `software` `single` `flat`), `groups[]` (`label`, `kind`, `items[]` with `label`, `bytes`, `files[]` (file numbers as in `info`), `selected`, `library` (`missing` `in_library` `other_quality`), `quality`, `this_mac`, `platform`), `files[]` (preselection per file), `selected_bytes`, `total_bytes`, `free_bytes`, `destination`, `summary` |
| `choose_files` | control | yes | `id` and exactly one of `accept_default`, `files[]` (download only these file numbers), `skip[]` (all but these) | `ActionDto`. A held torrent starts and downloads only those files; on a running torrent it changes which files are wanted. Selecting nothing is `invalid` (use `cancel_choice`) |
| `cancel_choice` | control | yes | `id` | `ActionDto`; removes a held torrent, nothing is left on disk, library files are never touched |
| `location_moves` | read_only | no | - | `items[]` (`id`, `name`, `bytes`, `from`, `to`, `cross_volume`, `kind`: `finished` `incomplete` `retarget`), `total_bytes`, `cross_volume`: torrents that sit elsewhere than the settings say (nothing moves) |
| `move` | **full** | yes | `ids`, `path?` (absolute or `~/...`; never a system, `~/Library` or hidden folder, like `add.save_path`) | `queued[]`, `refused[]` (`id`, `reason`). With `path` the torrents move below it and are pinned there; without it they move to where the settings and category say. Same volume: rename; other volume: verified copy while seeding goes on |
| `cancel_move` | control | yes | `ids` | `ActionDto`; stops queued and running moves |
| `chat.groups` | read_only | no | - | `groups[]` (`id`, `name`, `icon`, `position` from 1, `channels[]` (`id`, `name`): only those open to agents), `icons[]`. The sidebar's groups of channels on this computer (SPEC §7G.7 *Groups*): this device's own, never sent to another |
| `chat.group_new` | control | yes | `name` (new, in any case), `icon?` (one of `icons`; absent: the one its name suggests, "Agents" sparkles, else `folder`) | one group as in `chat.groups` |
| `chat.group_edit` | control | yes | `group` (id, a prefix of 4+, or name), `name?`, `icon?`, `position?` (from 1) | one group as in `chat.groups` |
| `chat.group_delete` | control | yes | `group` | `deleted`, `id`, `name`; its channels go back to the plain list, nothing else changes |
| `chat.set_group` | control | yes | `channel` (open to agents), `group?` (absent: none) | the channel as in `chat.list` |

The other `chat.*` and `sync.*` methods are described in `seedwell help chat` / `seedwell help sync` (crates/cli/src/help.rs)
and SPEC §7G.

### Error codes

`parse_error`, `invalid_request`, `unknown_method`, `invalid_params`, `permission_denied`
(`data: {required, current}`), `agent_access_off`, `hello_required`, `rate_limited`
(`data.retry_after_ms`), `too_many_connections`, `request_too_large`, `not_found`, `ambiguous`
(`data.candidates`), `duplicate`, `invalid` (well-formed but unacceptable; `data.errors[]` with field
paths for settings), `invalid_token`, `token_expired`, `unsupported`, `engine`, `internal`,
`shutting_down`, `timeout`, `unsupported_version`. Clients treat unknown codes as `other`.

### Events

`added`, `metadata_received`, `finished`, `state_changed` (`data.state`), `error` (`data.message`), `choice_needed` (`data.reasons`), `choice_resolved` (`data.confirmed`), `locations_changed` (`data.torrents`, `data.bytes`), `move_started` / `move_progress` / `move_finished` / `move_failed`,
`seeding_goal_reached`, `removed`, `auto_added`, `duplicate`, `direct_restarted`,
`watch_error`, `config_reloaded`, `config_rejected`, `lagged` (events were missed). `peer_banned` and
UI-only engine events are not forwarded.

### Evolving the protocol

New methods and new optional fields do not change `protocol`. A breaking change bumps
`PROTOCOL_VERSION`; the server answers `unsupported_version` to a request whose `v` it does not speak.
To expose a new engine field: add it to the DTO in `types.rs` (`#[serde(default)]`) and fill it in
`engine_backend::row_dto`/`detail_dto`; nothing else changes.

## Dependencies

`seedwell-state` (validation, stats, profiles, redaction), `seedwell-classify` (category names), `seedwell-engine`
(feature `engine`), `serde`/`serde_json`, `tokio` (`net`, `fs`, `sync`, `time`, `io-util`), `getrandom`
(tokens, temp names). Base64 is hand-rolled (30 lines) instead of pulling a crate.

## Tests

`cargo test -p seedwell-control`

* `src/**`: unit tests (protocol round trips, id resolution, filters, validation, settings deny-list and routed keys,
  redaction, token expiry/reuse/ownership, socket modes, `peer_allowed`, rate limiter, line reader).
* `tests/server.rs`: the server against an in-memory backend on a real socket: **permission matrix
  (every method x every level)**, permission-before-params, off/level changes on open connections,
  two-step remove (reuse, wrong token, expiry, foreign client, level drop between steps), size caps and
  garbage, rate limit, 8-connection cap, 8 concurrent clients x 60 mixed calls, events (push, filter,
  poll, unsubscribe), activity log and redaction, settings, limits persistence, statistics, `wait_for`.
* `tests/decoder.rs`: deterministic mutation fuzzing of the request decoder and base64 decoder.
* `fuzz/`: `cargo +nightly fuzz run decode_request` (corpus seeds in `fuzz/corpus/decode`).
* The end-to-end tests with real engines live in `crates/cli/tests`.
