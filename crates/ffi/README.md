# trs-ffi

The UniFFI surface between the Rust core and the Swift app (`app/`). One object, `Core`, plus plain
records. `unsafe` is not used here; the engine's `#![forbid(unsafe_code)]` stands behind it.

```
Swift (AppKit/SwiftUI) ── Core ─┬─ RealBackend ── trs_engine::Host (own runtime) ── EngineHandle
                                │       └─ trs_control::start_for_app  (agent socket + activity log)
                                ├─ MockBackend   (--mock: a simulated library for design work)
                                └─ SettingsStore (config.toml / rules.toml / trackers.toml via trs-state)
```

## Rules of the boundary

* The UI **pulls**: `snapshot()` about once a second while a window or the menu is open. Rows are
  cached for 400 ms in the core, so several views asking in the same second cost one engine round trip.
  Nothing polls while nothing is visible; the core pushes only rare events through `CoreListener`.
* Lists that can be long are **paged**: `sharing_rows(RowQuery)` filters (group, tracker, text) and pages the
  finished torrents, `sharing_counts` counts them. The snapshot itself carries only the downloads, the queue,
  the recent list and the eight busiest seeds.
* Settings, rules and tracker profiles are **typed records** (`AppSettings`, `RulesDoc`, `ProfileRecord`).
  A save is load, overwrite the fields the record knows, validate, write: unknown keys and fields this layer
  does not edit survive. The engine's `Host` applies the files by itself (2 s poll) and reports
  `on_settings_changed` / `on_settings_rejected`. Validation problems come back as `CoreError::Invalid` with
  one `path: message` per problem.
* `Core::new(CoreOptions)`: `mock`, `allow_loopback_trackers` (QA only) and `forbidden_root` (an isolated run
  refuses to start when its state or any folder of its `config.toml` lies inside the real home).
* `shutdown(deadline_secs)` blocks (call it off the main thread): 120 s for Quit, `shorten_shutdown(3)` at
  logout (SPEC §12 #16).

## Where things come from

| FFI | Engine |
|---|---|
| `TransferRow.seed_group`, `error`, `retry_in_secs` | `TorrentRow.seed_group`, `error`, `next_retry_ms` |
| `TransferRow.awaiting_choice`, `moving_percent` | `TorrentState::AwaitingChoice`, `TorrentRow.moving` |
| `Stopped` group | finished + paused: not one of the five groups of the spec, but it needs a place |
| `TransferRow.pending` (`PendingStart`), `pending_pause` / `pending_resume` / `pending_start_now` / `pending_choose`, `CoreListener::on_pending_changed`, `TransferSettings.start_delay_secs` | `TorrentRow.pending`, `EngineHandle::pending_pause` / `pending_resume` / `start_now` / `pending_choose`, `EngineEvent::Pending`, `transfers.start_delay_secs` (SPEC 7A.7); the row's state is `Queued` |
| `choice` / `confirm_choice` / `cancel_choice` | `EngineHandle::choice` / `reopen_choice`, `confirm_choice`, `cancel_choice` |
| `location_moves` / `move_torrents` / `move_to` | `EngineHandle::location_moves`, `move_torrents`, `move_to` |
| `watch_backlog` | `EngineHandle::watch_backlog` |
| `on_agent_activity`, `agent_activity` | `trs_control` activity sink and ring |

`network.allow_lan_trackers` is a real `trs-state` field (`config.toml`, `[network]`, off by default); the
engine reads it at start. A file written when the key was still an unknown key reads the same.

## Display names, rule and goal text

`TransferRow.display_name` (also on `TorrentInfo` and `ChoiceInfo`) is the clean title lists show ("Sintel (2010)",
"Fallen Kingdom — S02"); `name` stays the raw release name for Get Info, tooltips and search (the Sharing filter and
the app's search match both). The engine computes it **once per torrent** (`trs_classify::torrent_title`, file-aware:
the classifier sees the file list and a single-file torrent takes its file's name when that says more), stores it with
the torrent record, and recomputes it only when the category changes; `TorrentRow::display_title()` is what this layer
passes on, so the CLI (`seedwell list`, `info`) show the same text (JSON `title` next to `name`). Torrents stored
before titles existed get theirs in one background pass after start.

The same goes for the words about rules and goals: `SeedGoal::describe()` ("Seed to ratio 1.0"), `then_phrase()`
("seeds until ratio 1.0") and `ChoicePreview::summary()` ("Rule Y · Saves to ~/Media/Movies · Then seeds until ratio
1.0") live in the engine. `ChoiceInfo.rule_name` / `then_text` / `summary_text` and `TorrentInfo.seeding_goal` /
`goal_is_custom` are those, not strings built in Swift. `TransferRow.seed_secs` is the engine's seeded time (all
stretches). `SeedingGoal::RuleDefault` calls `EngineHandle::clear_seed_goal` (the CLI has `seedwell goal <id>
--rule-default`).

## Tests

`cargo test -p trs-ffi`: settings round trips and validation, rule/profile ids and passkey stripping, paging and
filtering on the mock, rule preview counts, and a real `Core` that starts, snapshots and stops in a temp dir.
