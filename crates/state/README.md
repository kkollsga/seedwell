# trs-state

Everything Seedwell keeps on disk, behind plain engine-agnostic types (`[u8; 20]` info-hashes, no
dependency on `trs-engine`):

| File (in `~/Library/Application Support/Seedwell/`) | What | Module |
|---|---|---|
| `config.toml` | every setting of SPEC §8.4 (version 2) | `config` |
| `rules.toml` | seeding rules + the Default rule (SPEC §7) | `rules` |
| `trackers.toml` | tracker profiles (SPEC §7E.1) | `trackers`, `url` |
| `state.redb` | torrents, resume data, counters, tracker timing, daily statistics, peers cache, DHT table (schema 2) | `store`, `record`, `stats`, `timing` |
| `torrents/<infohash>.torrent` | original metainfo bytes | `Store::put_torrent_file` |
| `config.toml.bak`, `backups/` | last good version, last 5 timestamped versions, DB backups before migrations | `doc`, `store` |
| (no file) | same-volume check and the "incomplete and finished are on different disks" warning | `volume` |

Design rules: files on disk are untrusted (no panics, size caps everywhere), human-edited files are
hot-reloaded and validated with field-path errors, binary records are versioned and compact, and
nothing is ever deleted to "fix" a problem (damaged files are moved aside).

## API tour

```rust
use trs_state::*;

// --- the three TOML documents -------------------------------------------------------------
let dir = std::path::Path::new("/path/to/Seedwell");
let cfg_store = DocStore::<Config>::new(dir);
let loaded = cfg_store.load();           // never fails: file, else last-good backup, else defaults
if let Some(err) = &loaded.error { /* show: "config.toml: ... using the last good copy" */ }
let mut cfg = loaded.value;
cfg.network.listen_port = 51413;
cfg_store.save(&cfg)?;                   // validate, back up the old file, temp+fsync+rename

// hot reload: one stat() per poll (every 2 s); invalid edits never replace the running config
let poller = Poller::new(cfg_store.clone(), cfg.clone());
let _watch = spawn_watcher(poller, doc::POLL_INTERVAL, |ev| match ev {
    PollEvent::Reloaded(p) => { /* apply p.value, show p.warnings */ }
    PollEvent::Rejected(e) => { /* show e: "network.listen_port: must be 0 (random) or 1024-65535" */ }
})?;

// rules and tracker profiles
let rules = DocStore::<RuleSet>::new(dir).load().value;
let profiles = DocStore::<TrackerProfiles>::new(dir).load().value;
let m = profiles.match_announce(["https://tracker.example.org/a/KEY/announce?x=y"]);   // Option<ProfileMatch>
let bucket = profiles.bucket_for(announce_urls, private);                          // Profile(id) | Public | OtherPrivate
let policy = rules.resolve(&TorrentAttrs {
    tracker: m.map(|m| m.profile.id.as_str()),
    linked_rule: m.and_then(|m| m.profile.rule.as_deref()),
    private: true,
    category: trs_classify::Category::Tv,
});                                                                                  // ResolvedPolicy, pure
let dangling = cross_check(&cfg, &rules, &profiles);                                // Vec<Warning>

// --- the database -----------------------------------------------------------------------
let Opened { store, report } = Store::open(dir)?;       // report.recovered / .migrated / .created
store.put_torrent_file(&hash, &torrent_bytes)?;           // atomic, immediate
store.put_torrent(hash, &TorrentRecord::new("name", size, now))?;   // batched (2 s debounce)
store.put_resume(hash, &resume)?;
store.add_counters(hash, Counters { downloaded: 0, uploaded: 4096, seed_secs: 5 })?;
store.record_stats(&StatsScope::Profile("tl".into()), Day::from_unix(now, tz_offset), &StatsBucket { uploaded: 4096, ..Default::default() })?;
store.flush_now()?;                                       // commit and wait
let mut timing = store.timing_handle();                  // impl trs_tracker::TimingStore, durable save()
let series = store.stats_series(&StatsScope::All, Day::from_unix(now, tz_offset), 30)?;
```

Locations and volumes (SPEC §7A.4):

```rust
use trs_state::volume::{same_volume, volume_id, cross_volume_pairs, CrossVolume, LocationScope};
same_volume(Path::new("~/x"), Path::new("/Volumes/Media/y"))?;   // io::Result<bool>: st_dev of the nearest EXISTING ancestor
let warnings: Vec<CrossVolume> = cross_volume_pairs(&cfg);       // every incomplete -> finished pair on different disks
```

Key signatures:

```rust
pub struct Store { /* Send + Sync */ }
impl Store {
    pub fn open(dir) -> Result<Opened, StateError>;
    pub fn open_with(dir, &OpenOptions) -> Result<Opened, StateError>;
    pub fn flush_now(&self) -> Result<(), StateError>;
    pub fn close(self) -> Result<(), StateError>;
    pub fn put_torrent(&self, InfoHash, &TorrentRecord) / remove_torrent(&self, InfoHash) / load_torrents() -> LoadedTorrents / get_torrent
    pub fn put_torrent_file(&self, &InfoHash, &[u8]) / read_torrent_file / list_torrent_files
    pub fn put_resume(&self, InfoHash, &ResumeData) / remove_resume / get_resume
    pub fn add_counters(&self, InfoHash, Counters) / set_counters_at_least / get_counters / load_counters
    pub fn intern_root(&self, &str) -> Result<u32, _> / root_path(u32)
    pub fn put_peers(&self, InfoHash, &[SocketAddr]) / get_peers
    pub fn put_dht_table(&self, &[u8]) / get_dht_table
    pub fn record_stats(&self, &StatsScope, Day, &StatsBucket)
    pub fn stats_series(&self, &StatsScope, today: Day, days: u32) -> Vec<(Day, StatsBucket)>
    pub fn stats_summary(&self, &StatsScope, today: Day) -> StatsSummary   // today / week / month / all time
    pub fn stats_total(&self, &StatsScope) -> StatsBucket / stats_scopes()
    pub fn timing_handle(&self) -> TimingHandle;                            // impl TimingStore
    pub fn check_file(path) -> Result<bool, _>;                             // offline integrity check
}
pub trait Document { fn defaults() / parse_str(&str) -> Result<Parsed<Self>, DocError> / render(&self) -> Result<String, DocError> }
impl<T: Document> DocStore<T> { new(dir) / load() -> Loaded<T> / read_strict() / save(&T) / backups() }
pub struct Poller<T>; pub fn spawn_watcher(..) -> io::Result<WatchHandle>;
impl RuleSet   { resolve(&TorrentAttrs) -> ResolvedPolicy; find; count_affected; add_rule / update_rule / remove_rule / move_rule / set_enabled / set_default }
impl TrackerProfiles { match_announce; bucket_for; add / update / remove / add_url / remove_url / set_rule }
```

## `config.toml`

One table per Settings tab, struct order on disk, `version = 1` first. Rates are bytes/s with `0`
meaning unlimited. Durations are `"90s"`, `"15m"`, `"72h"`, `"7d"` (or integer seconds). The full
default file is in `fuzz/corpus/config/default.toml`; an excerpt:

```toml
version = 1

[general]            # General tab
on_battery = "limit" # keep_going | limit | pause
[transfers]          # Transfers tab
download_limit = 0
upload_limit = 5000000
max_active_downloads = 3
max_connections_per_host = 4
ask_before_adding = "multi_file"        # never | multi_file | always
[transfers.alt]      # "Limit Speed" (alternative limits)
enabled = false
download_limit = 2000000
upload_limit = 500000
[transfers.alt.schedule]
enabled = true
days = "weekdays"    # "weekdays" | "weekends" | "every_day" | ["mon", "wed"]
from = "08:00"
to = "17:00"         # earlier than `from` = ends next morning
[folders]            # Folders tab
download_dir = "~/Downloads"
incomplete_dir = "~/Downloads/.incomplete"
by_status = false                       # organize finished torrents into Active/ Dormant/ Stopped/ Dead/ (SPEC 7A.3)
torrent_after_add = "trash"             # trash | archive (needs archive_dir) | leave
[[folders.watch]]
path = "~/Torrents/Books"
mode = "ask"                            # auto | ask
after_add = "archive"
archive_dir = "~/Torrents/Done"
category = "book"                       # forced category (optional)
rule = "books"                          # forced rule id (optional)
[detection]
confidence_threshold = 0.8
[network]            # Network tab
listen_port = 0                         # 0 = a random port at every start (never written back); a number = manual choice
dht = true  pex = true  lsd = true      # (one per line in the real file) public torrents only
upnp = true  natpmp = true  utp = true  ipv6 = true
encryption = "prefer"                   # off | prefer | require
allow_lan_trackers = false              # contact trackers on a private network (LAN/VPN); loopback stays refused
[agents]             # Agents tab
permission = "off"                      # off | read_only | control | full
[advanced]           # Advanced tab
write_cache_mib = 32
preallocation = "sparse"
[advanced.tiers]
hot_to_warm = "5m"
warm_to_cold = "5m"
[advanced.logging]
level = "info"
```

**Version 2** (this build) adds, all optional with defaults, so a version 1 file reads as it is:

```toml
version = 2
[transfers]
large_torrent_gb = 50              # SPEC 7A.6 "very large": hold a torrent over this size for a choice (0 = off)
start_delay_secs = 10              # SPEC 7A.7: seconds an auto-added torrent is held, silent, before it starts (0 = off, max 600)
[advanced]
move_limit_mib = 0                 # speed limit of a cross-volume copy when files move (SPEC 7A.5), 0 = unlimited
developer_mode = false             # log every tracker exchange, secrets masked, to logs/trackers.log (SPEC 8.7)
[folders]
download_dir   = "~/Downloads"     # the app-level default FINISHED location (named download_dir since version 1)
incomplete_dir = "~/Downloads/.incomplete"   # the app-level default INCOMPLETE location
[folders.categories.tv]            # per-category overrides (SPEC 7A.4); either key may be left out; empty tables are not written
incomplete_dir = "/Volumes/Fast/.incomplete"
finished_dir   = "/Volumes/Media/TV"   # a plain folder; the torrent's own layout goes below it
[[folders.watch]]
path = "~/Downloads"
since = 1791000000                 # unix seconds the folder joined the watch list; only .torrent files that arrive later are picked up
```

Precedence of the finished location: rule `finished_dir` > the category's `finished_dir` > `download_dir`. All
are plain folders; the torrent's own layout goes below them (a multi-file torrent in a folder named like the
torrent). There are no destination templates and no placement modes (SPEC 7A.3, 2026-10-14).

**Migration 2 -> 3 (flat layout).** `folders.templates` and `folders.placement` are dropped, a template-valued
`folders.categories.<name>.finished_dir` becomes its fixed folder (the part before the first `{`; the override is
dropped when nothing is left), and a rule's `template` becomes `finished_dir` the same way. The rewrite is
done by `file_is_outdated` below, so the old file is backed up first. Torrents already placed keep their paths.

**Migration 1 -> 2.** `Document::file_is_outdated(text)` lets a store upgrade a valid old file: `DocStore::load`
parses it (every new key has a default), then saves it in the current format, which first copies the old
file to `config.toml.bak` and `backups/config.toml.<stamp>` like any save (a read-only folder leaves the
loaded value in force and reports the write error in `Loaded::error`). The same hook is there for rules and
trackers when they need one.

**Watch folder baselines.** `Config::stamp_watch_baselines(previous, now)` sets `since` on every entry that has
none, whose path was not in `previous` (new or edited entry) or that was disabled and is enabled now; it
returns whether anything changed so the caller saves. `trs_engine::Host` calls it at start-up (first launch,
upgrade) and on every reload, and rounds `now` up to the next whole second so a file that was there a moment
ago is never "newer" than the baseline.

Not in `config.toml` on purpose: the default seeding goal and "when a goal is reached" (they are the
`[default]` table of `rules.toml`, so the Settings shortcut edits that), tracker profiles,
client identity (not configurable, SPEC §7B.1), network-interface binding (v1.x).

Validation reports every problem with its path, e.g. `network.listen_port: must be 0 (random) or
1024-65535`, `folders.categories.movie.finished_dir: must be absolute or start with ~/`, `folders.watch[2].path:
duplicate watch folder ~/Torrents`. TOML syntax and type errors carry line and column. A `version`
newer than this build is refused (`Unsupported`), never half-read.

**Unknown keys** are never silently dropped. They produce a `Warning` with the key path, and
unknown keys in tables are kept and written back on the next save (so a downgrade does not erase
settings of a newer build). Unknown keys inside array elements (`[[rule]]`, `[[tracker]]`,
`[[folders.watch]]`) are warned about but not preserved.

**Saving** validates first, then: copy the current file to `config.toml.bak` and to
`backups/config.toml.<UTC stamp>` (newest 5 kept), write `.config.toml.tmp.*`, fsync, rename, fsync the
directory. Only a file that itself validates is copied into the backups, so a hand-broken file can
never push a good backup out (it is parked as `config.toml.rejected`). Saving identical content is a no-op.
Comments are not preserved when the app rewrites the file (the header says so).

**Loading never fails**: valid file; else (missing) defaults, written to disk; else (invalid) the newest
valid of `.bak`, `backups/*`, with the original error in `Loaded::error` and the broken file left in
place for the user to fix; else defaults.

**Hot reload**: `Poller::poll` is a single `stat` (mtime + size); `spawn_watcher` runs it every 2 s on
one thread. A changed file is parsed and validated; `Reloaded` is only emitted when the content
differs from the running value (so the app's own saves are silent), `Rejected` carries the error and
the running value stays in force. A broken file must survive one more interval before it is reported,
so an editor that is mid-write does not raise a false alarm.

## `rules.toml`

```toml
version = 1

[default]                                   # always last, cannot be deleted; every action defined
priority = "normal"
on_goal = "stop_keep"                       # stop_keep | remove_keep_files | remove_delete_files
goal = { kind = "ratio_or_time", ratio = 1.0, time = "7d" }
# goal kinds: stop_when_finished | ratio | time | ratio_or_time | ratio_and_time | forever

[[rule]]
id = "tz"                                   # a-z 0-9 - _ ; unique
name = "Z: priority, H&R safe"
enabled = true
match = { tracker = ["tracker-z"], private = true, category = ["tv", "movie"] }   # profile ids; "public" = no profile
priority = "high"                           # low | normal | high  (weights 1 : 2 : 4)
goal = { kind = "ratio", ratio = 2.0 }
min_seed_time = "72h"                       # hit-and-run floor: no goal counts before this
on_goal = "remove_keep_files"
finished_dir = "~/Seeding"                  # finished-location override, a plain folder (rule > category > default)
up_limit = 1000000                          # per-torrent, bytes/s, 0 = unlimited
```

**Evaluation order** (`RuleSet::resolve`, pure and total):
1. If the torrent's tracker profile links a rule (`Profile::rule`) that exists, is enabled and whose own
   `private`/`category` conditions hold, that rule applies (the link replaces its `tracker` condition).
2. Else the enabled rules in file order, **first match wins** (no "best match").
3. Else the Default rule.

A rule only states what it changes; every unset action comes from `[default]` field by field. Missing
conditions match everything. A rule that an earlier enabled rule fully covers is reported as a warning
("never matches"). Editing helpers (`add_rule`, `move_rule`, ...) validate and roll back on error.
`RuleSet::count_affected` backs the editor's "affects 48 torrents" preview.

## `trackers.toml`

```toml
version = 1

[[tracker]]
id = "tl"                                   # stable; used by rules, torrent records and statistics
name = "Example Tracker"
urls = ["https://tracker.example.org/a/*/announce", "backup.example.org", "*.example-mirror.org"]
rule = "tz"                                 # optional: rule applied to this tracker's torrents
color = "#ff8800"                           # optional
icon = "flame"                              # optional SF Symbol
```

**Passkeys are secrets and are never stored.** `UrlPattern::parse` drops the query string and turns
passkey-looking path segments (20+ alphanumerics with a digit) into `*`. `AnnounceUrl` (the torrent
side) keeps no query at all, so its `Display`/`Debug` print `https://host/a/***/announce?***` by
construction; `redact_url(&str)` does the same for raw strings and never echoes unparseable input.
Parse errors never contain the offending URL.

**Matching precedence** (`TrackerProfiles::match_announce`): every announce URL of the torrent is tried
against every pattern of every profile. A pair has a tier: 1 = scheme + host + port + path equal (default
ports ignored, `*` matches one path segment); 2 = host + path equal; 3 = host only (bare host, `*.suffix`
glob, or a pattern whose path differs). The best pair wins, ties broken by: exact host over glob; more
literal path segments (only when the path matched); earlier URL in the torrent; earlier profile in the
file; smaller id. No match: `Public` (or `OtherPrivate` for private torrents; `bucket_for`).
`trackers::group_of_host(host)` identifies a tracker automatically (registrable domain of the primary announce host, `tracker.example.org` -> `example.org` / `Example`); nothing is stored for it. `TrackerProfiles::add_favorite(name, hosts)` stores a favorite: bare hosts only, never a path or passkey; saving twice returns the same profile. SPEC 7E.1.
Duplicate patterns across profiles are accepted with a warning. Reserved ids: `public`, `other-private`, `all`.

## `state.redb`

redb tables (`TableDefinition` names as in docs/ENGINE.md §7):

| Table | Key | Value |
|---|---|---|
| `meta` | `"schema"` | u32 LE schema version |
| `torrents` | `[u8;20]` | `TorrentRecord` v2 (v1 is still read) |
| `resume` | `[u8;20]` | `ResumeData` v1 (incomplete torrents only) |
| `counters` | `[u8;20]` | lifetime `{downloaded, uploaded, seed_secs}`, three varints |
| `tracker_timing` | info-hash ‖ announce URL bytes | `TimingRecord` v1 (flags + up to 3 varints + tracker id) |
| `stats_daily` | `(profile key, local day u32)` | `StatsBucket` v1 |
| `stats_totals` | profile key | lifetime `StatsBucket` |
| `peers_cache` | `[u8;20]` | up to 256 peers, 7 (v4) / 19 (v6) bytes each |
| `dht_table` | `"table"` | opaque blob ≤ 1 MiB |
| `save_roots` | `u32` | interned save-folder path |

Profile keys are the profile id, `@public` or `@other-private`. Removing a torrent deletes its rows
(record, resume, counters, peers, timing) and its `.torrent`, never its statistics.

**`TorrentRecord` v2** adds `stage_root` (interned root of the torrent's staging folder, `<root>/<info-hash>/`;
`None` = below the save root's `.incomplete`, the v1 meaning) and two flag bits: `TorrentFlags::AWAITING_CHOICE`
(bit 6, SPEC 7A.6: held until the user chooses) and `LOCATION_PINNED` (bit 7: the user chose where it lives,
so setting changes leave it alone). v2 costs one byte (`opts2`) plus the root id when set.

### Sizes (measured, Apple M-series, release)

* `TorrentRecord`: version byte, flag varint, option bitmap, then varints and length-prefixed strings.
  A typical record (60-byte name, profile `example`, category, completed) is **89 bytes** (+ 20-byte key).
  Worst case is bounded by the caps (name 1 KiB, rel_path 4 KiB, ids 64 B, skipped-files bitset 32 KiB).
* `StatsBucket`: 64 bytes in memory, about 9 bytes empty to ~35 bytes busy on disk (version + 8 varints).
  10 years x 20 profiles = 73k rows, ~3 MB.
* `Counters`: 4-12 bytes. `TimingRecord`: 2-30 bytes. `ResumeData`: `pieces/8` + 2 B per partial piece + ~10 B per file.
* `state.redb` file: ~1.5 MiB empty (redb preallocates), **3.6 MiB with 1000 torrents + counters**
  (3.6 KB/torrent on disk including copy-on-write slack; the data itself is ~140 B/torrent).
* **1000 torrents: `load_torrents()` 0.2 ms, `load_counters()` 0.07 ms** (budget 50 ms; `tests/perf.rs`).
  `Store::open` takes ~50 ms for that file because it verifies every page checksum (`OpenOptions::verify`).
* Resident memory: redb page cache is capped at 4 MiB (`db_cache_mib`; redb's default is 1 GiB). The
  write batch holds one entry per distinct key, not per call.

### Write path and durability

Calls coalesce into an in-memory batch; a writer thread commits it in one transaction after the 2 s
debounce. `flush_now()` commits immediately and waits. `Drop`/`close()` flush. All commits are redb
`Immediate` (fsync). A failed commit (disk full, I/O error) keeps the batch, merges newer calls into it and
retries after the next debounce; `last_write_error()` and `flush_now()` expose it. Read methods flush first,
so they always see the caller's own writes; they are meant for startup and UI selections, not per-packet use.

The tracker's `completed_sent` write goes through `TimingHandle::save`, which runs **its own committed
transaction before returning**. If `load` cannot read a row it answers `completed_sent = true` (a missed
`completed` is safe, a duplicate one is a ban reason). The handle holds only a weak reference: after the
`Store` is dropped, calls fail with "store closed" instead of keeping the file open.

Counters only grow: `add_counters` adds with saturation; `set_counters_at_least` takes the field-wise max.

### Schema versions and migrations

`store::SCHEMA` = `{ current: 2, migrations: [1 -> 2] }`. Version 2 only changed the torrent record (below):
a v1 record decodes as it is, so the step rewrites nothing, but bumping the schema makes the upgrade take
its backup (`backups/state.redb.v1.<stamp>`) and stops older builds from opening a file whose records they
would misread. Opening:
* newer file than this build: `StateError::NewerSchema`, nothing is modified or moved;
* older file: the database is closed, copied to `backups/state.redb.v<N>.<stamp>`, reopened, all steps run in
  **one** write transaction together with the version bump (a failing step leaves the file as it was);
* a missing step: `NoMigration`.

Add a migration by appending to `SCHEMA.migrations` (`Migration { from, to, name, run }`) and raising `current`.

### Seedwell 0.1 databases (redb 2 file format)

Seedwell 0.1 wrote `state.redb` with redb 2.6 (file format v2). redb 3 dropped that format and re-encoded
variable-width tuple keys (`stats_daily` is keyed by `(&str, u32)`), so redb 4 refuses the file with
`UpgradeRequired`. Open then converts it: the file is copied to `backups/state.redb.redb2.<stamp>`, every
table is read with redb 2.6 and written to `state.redb.convert` with redb 4, and that file is renamed over
`state.redb`. A crash at any point leaves the old file or the new one, never neither. `OpenReport::converted`
names the backup. The `meta` table comes across too, so schema migrations then run as usual. A 0.1 file that
is also damaged is set aside like any damaged file; a conversion that fails otherwise (`StateError::Legacy`)
leaves the file alone.

### Corruption handling

* Garbage / truncated / mutated `state.redb` that redb reports as corrupt, or that makes it panic
  (see below): the file is moved to `state.redb.corrupt-<stamp>`, a fresh database is created, and
  `OpenReport::recovered` says why. `Store::list_torrent_files()` lets the engine rebuild its torrent
  list from `torrents/*.torrent`. Other failures (permissions, already open elsewhere, newer schema)
  never move anything.
* **redb can panic on damaged files** (seen in 2.6 and 3.1) (an `assert!` in the page manager, unchecked slice
  indexing in the allocator). Two defences: `catch_unwind` around open and the integrity check, and, because
  the release profile uses `panic = "abort"`, an `opening` marker file: if opening aborts the process, the
  next launch retries once, and a second consecutive abort sets the file aside. A crash loop is therefore
  limited to two launches. Open also runs `check_integrity()` (checksums of every page) so latent damage
  surfaces inside the guarded section. A new database is created as `state.redb.new` and renamed into place,
  so a crash during the very first run cannot leave a half-built file.
* A record that fails to decode is skipped and reported (`LoadedTorrents::skipped`), never deleted.
* `config.toml` / `rules.toml` / `trackers.toml`: see "Loading never fails" above.

## Statistics (SPEC §7E.2)

`record_stats(scope, day, delta)` adds to the day bucket and to the lifetime row in the same transaction,
so `sum(daily) == total` always. `stats_series(scope, today, n)` returns `n` dense days (oldest first);
`stats_summary` returns today / this week (Monday on) / this month / all time; `StatsBucket::ratio()` is
`uploaded / downloaded`; `torrents_completed` and `files_completed` are the "downloaded torrents / files"
counts; `stats::series_to_csv` is the CSV export. `StatsScope::All` sums every bucket, including deleted profiles.

**Day boundaries** are local civil days (`Day` = days since 1970-01-01 in the user's zone). The crate has no
time-zone data: callers pass the zone's UTC offset valid *at that instant* to `Day::from_unix(secs, offset)`,
which keeps DST handling with the OS and makes tests deterministic. Weeks start on Monday.

## Dependencies

| Crate | Why |
|---|---|
| `redb` 4.x | the transactional store; pure Rust, copy-on-write, crash safe |
| `redb` 2.6 (as `redb2`) | reads a Seedwell 0.1 database once to convert it (`src/legacy.rs`); can go once no 0.1 files are left |
| `serde` (derive) + `toml` | TOML documents; `toml` with `preserve_order` (uses `indexmap`, already in the tree via `toml_edit`) so files keep the struct order |
| `trs-tracker` (no default features) | the `TimingStore` trait only; none of the async transports are built |
| `trs-classify` | the `Category` enum |
| dev: `proptest` | property tests (same as the other crates) |

No `tempfile`, `libc`, `chrono` or logging dependency: temp directories, civil-date maths (Hinnant) and
SIGKILL (`Child::kill`) use std.

## Tests

`cargo test -p trs-state` (unit + integration), `cargo clippy -p trs-state --all-targets -- -D warnings`.

* `tests/store.rs`: persistence, batching, removal, statistics, timing, migrations, newer schema, failure
  retry (`__inject_commit_failures`), garbage/truncated/mutated database files
  (`STATE_DAMAGE_ROUNDS=600 STATE_DAMAGE_SEED=n` for a long run).
* `tests/crash.rs`: a child process writes counters, statistics, torrent records, durable timing rows and is
  `SIGKILL`ed at random moments (5-405 ms after spawn, so also while opening and while committing); the
  parent reopens and checks no recovery happened, the integrity check passes, acknowledged flushes and durable
  timing writes are present, counters are monotonic and counters/statistics (one transaction) agree.
  `CRASH_ITERS=1000 CRASH_SEED=n cargo test -p trs-state --test crash -- --nocapture`. This proves process-crash
  safety; power-loss safety rests on redb's fsync'd two-phase commit. Recorded runs: 2 x 1000 SIGKILLs (seeds
  987654321 and 424242; 835-839 kills landed after the child had acknowledged work, the rest during startup),
  ~17 000 durable timing writes verified per run, no corruption, no lost acknowledgement, no counter regression.
* `tests/props.rs`: config / rules / trackers round trips, `resolve` against an independent oracle, tracker match
  determinism, passkey non-leak, schedule and calendar maths, "never panics" on arbitrary and mutated text and bytes.
* `tests/docs.rs`: load/save/backup rotation/fallback/hot reload. `tests/perf.rs`: size and speed budgets.
* `fuzz/` (`cargo +nightly fuzz run config|rules|trackers|records`, seeds in `fuzz/corpus`). A redb-file fuzz
  target is deliberately absent: libFuzzer aborts on any panic, and redb's known panics on damaged files are
  handled by the guards above and exercised by the mutation test instead.

## Known limitations

* Comments in the TOML files are not preserved when the app rewrites them.
* Unknown keys inside array elements are warned about but dropped on save.
* The database file never shrinks (redb reuses freed pages; no compaction call is exposed yet).
* `Store` read methods block on a flush; do not call them from latency-sensitive paths.
