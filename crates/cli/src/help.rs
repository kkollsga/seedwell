//! The one table of commands: usage, summary, examples and JSON shape. `seedwell help`, `seedwell <cmd>
//! --help`, `seedwell help agents` and the embedded skill (`seedwell skill show`) are all generated from it,
//! and the tests parse every example, so the text cannot drift from the parser.

/// One command in the table.
pub struct CmdHelp {
    /// The word the user types.
    pub name: &'static str,
    /// Section in the overview.
    pub group: &'static str,
    /// Usage line(s), after `seedwell `.
    pub usage: &'static str,
    /// One line.
    pub summary: &'static str,
    /// More text for `--help` (may be empty).
    pub details: &'static str,
    /// Concrete example command lines (each must parse).
    pub examples: &'static [&'static str],
    /// The `--json` shape in one line (`v` is always 1 and always present).
    pub json: &'static str,
    /// Permission level the command needs: `read`, `control`, `full` or `none`.
    pub needs: &'static str,
}

/// The exit codes, as documented everywhere.
pub const EXIT_CODES: &str = "\
0 ok
1 error (the app said no, or something failed)
2 usage (bad arguments; see `seedwell <command> --help`)
3 not found or ambiguous (a torrent selector matched nothing, or several; the candidates are listed)
4 permission denied or agent access off (Settings > Agents)
5 Seedwell is not running (use --launch to start the app)
6 timed out (`wait`)";

/// Units as accepted and printed.
pub const UNITS: &str = "\
Rates and sizes accept 5MB/s, 500KB/s, 1.5GB, 700MB, 4096 (bytes) and `unlimited` (rates). Decimal units (1 MB = 1,000,000 bytes).
Durations accept 90s, 15m, 12h, 7d, 2w. File numbers accept 0,3-5. Human output prints units; --json prints bytes, bytes/s and Unix seconds.";

const SEL: &str = "<torrent> is an id prefix (6+ hex characters), part of the name, or a filter: --all, --tracker X, --state S, --category C. A name that matches several torrents is an error (exit 3) that lists them.";

macro_rules! c {
    ($name:literal, $group:literal, $usage:literal, $summary:literal, $details:expr, [$($ex:literal),* $(,)?], $json:literal, $needs:literal) => {
        CmdHelp { name: $name, group: $group, usage: $usage, summary: $summary, details: $details, examples: &[$($ex),*], json: $json, needs: $needs }
    };
}

/// Every command, in the order of the overview.
pub const COMMANDS: &[CmdHelp] = &[
    c!("status", "Look", "status", "speeds, counts, limits, whether the port is open, and your permission level", "", ["seedwell status", "seedwell status --json"],
        r#"{"v","server","protocol","permission","totals":{"down_rate","up_rate","connections","listen_port","hot","warm","cold"},"counts":{"<state>":n},"total","limits":{"down_limit","up_limit","alt_active","alt_scheduled","alt_down_limit","alt_up_limit"},"summary","reachability":{"state" (open|mapped|checking|closed|unknown),"summary","port","mapping","method","external_port","behind_cgnat","router_unavailable","incoming","last_incoming_at"}}"#, "read"),
    c!("list", "Look", "list [state] [--state S] [--tracker X] [--category C] [--text T] [--kind torrent|direct] [--limit N] [--offset N]",
        "the torrents, filtered by state, tracker, category or name",
        "States: downloading queued seeding low-power paused error finished recent checking metadata awaiting-choice pending.",
        ["seedwell list", "seedwell list downloading", "seedwell list --tracker tl --category tv --limit 20"],
        r#"{"v","total","offset","transfers":[{"id","name","title","kind","state","progress","total_bytes","wanted_bytes","done_bytes","down_rate","up_rate","peers","seeds","eta_secs","ratio","tracker_host","private","category","error","waiting_note","pending",...}]}"#, "read"),
    c!("info", "Look", "info <torrent> [--no-files] [--no-peers] [--no-trackers]", "one torrent: state, files, trackers, peers", SEL,
        ["seedwell info a1b2c3", "seedwell info \"Sintel\" --no-peers"],
        r#"{"v","transfer":{...as list},"files":[{"index","path","length","done","priority","padding"}],"trackers":[{"url","tier","status","state","failures","seeds","leechers","last_announce_at","next_announce_at"}],"peers":[{"addr","client","outgoing","down_rate","up_rate","progress","choking_us","choked_by_us","interested","utp"}],"piece_count","piece_length","comment"}"#, "read"),
    c!("add", "Add and hold", "add <magnet|url|file.torrent>... [--save-path DIR] [--paused] [--category C] [--priority P] [--rule R] [--top] [--now] [--sha256 HEX] [--peer HOST:PORT]",
        "add a torrent, magnet link or direct download",
        "A new torrent is held a few seconds (and a multi-item one waits for you): confirm it with `seedwell start`, dismiss it with `seedwell cancel`. --now skips the wait. --save-path needs permission full.",
        ["seedwell add ~/Downloads/some.torrent", "seedwell add 'magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567' --category tv --paused", "seedwell add https://example.com/big.iso --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"],
        r#"{"v","id","name","kind":"torrent|direct","state","needs_choice","pending":{"starts_at_ms","starts_in_secs","paused"}|null,"choice":{...}|null}"#, "control"),
    c!("start", "Add and hold", "start <torrent>... [--suggested | --files LIST | --skip LIST]",
        "confirm a held torrent (the app's Start): it begins downloading", "Like Start in the app, a torrent waiting for a choice begins with every file ticked; --files downloads only those, --skip all but those, --suggested the engine's preselection (what the library does not have, this Mac's build). A torrent in its start delay starts at once.",
        ["seedwell start sintel", "seedwell start 3f2a9c --files 0-9,12", "seedwell start season --skip 14-20"],
        r#"{"v","action":"started","count","transfers":[{"id","name"}]}"#, "control"),
    c!("cancel", "Add and hold", "cancel <torrent>... [--keep-file]",
        "dismiss a held torrent (the app's Cancel): it is removed and its .torrent file goes to the Trash",
        "--keep-file keeps the .torrent file, like Shift-Cancel in the app. A magnet link or web address has no file. Only works on held torrents; use `remove` for running ones.",
        ["seedwell cancel sintel", "seedwell cancel 3f2a9c --keep-file"],
        r#"{"v","action":"cancelled","count","transfers":[{"id","name"}],"torrent_file":"trashed|kept|none","trashed_file"}"#, "control"),
    c!("choice", "Add and hold", "choice <torrent>", "what a held torrent contains: seasons, films, albums, builds, what the library has, and the preselection", SEL,
        ["seedwell choice sintel"],
        r#"{"v","summary","rule_summary","structure","reasons":[{"kind","text"}],"groups":[{"label","items":[{"label","bytes","files","selected","library","quality","this_mac"}]}]}"#, "read"),
    c!("pause", "Control", "pause <torrent>... | --all", "pause torrents", SEL, ["seedwell pause sintel", "seedwell pause --all", "seedwell pause --tracker tl --state seeding"],
        r#"{"v","action":"paused","count","transfers":[{"id","name"}]} (--all: {"v","action","all":true,"count"})"#, "control"),
    c!("resume", "Control", "resume <torrent>... | --all", "resume torrents", SEL, ["seedwell resume sintel", "seedwell resume --all"],
        r#"{"v","action":"resumed","count","transfers":[{"id","name"}]}"#, "control"),
    c!("top", "Control", "top <torrent>...", "move torrents to the front of the queue", SEL, ["seedwell top sintel"], r#"{"v","action","count","transfers"}"#, "control"),
    c!("priority", "Control", "priority <torrent>... high|normal|low", "queue and bandwidth priority", SEL, ["seedwell priority sintel high", "seedwell priority --state downloading low"], r#"{"v","action","count","transfers"}"#, "control"),
    c!("category", "Control", "category <torrent>... movie|tv|music|audiobook|book|software|other|none", "set the category (none = detect again)", SEL, ["seedwell category sintel movie"], r#"{"v","action","count","transfers"}"#, "control"),
    c!("files", "Control", "files <torrent> [--want LIST] [--skip LIST] [--only LIST] [--low LIST] [--normal LIST] [--high LIST]",
        "show a torrent's files, or choose which to download (numbers from `info`)", "--want and --normal download the listed files, --skip leaves them out, --only wants exactly these and skips the rest. Without options the files are listed.",
        ["seedwell files sintel", "seedwell files sintel --skip 3,5-7", "seedwell files sintel --only 0-2 --high 0"],
        r#"{"v","action","count","transfers"} (list: as info)"#, "control"),
    c!("goal", "Control", "goal <torrent>... [--ratio R] [--time D] [--min-time D] [--all-conditions] [--at-goal stop|remove|remove-and-delete] | --forever | --stop-on-finish | --rule-default",
        "when a torrent stops seeding", "remove-and-delete needs permission full.", ["seedwell goal sintel --ratio 2 --time 7d --at-goal stop", "seedwell goal --all --rule-default"], r#"{"v","action","count","transfers"}"#, "control"),
    c!("reannounce", "Control", "reannounce <torrent>... [--force] [--wait]", "ask the trackers again now", "Seedwell's own wait after failed tries never holds this back (only ten seconds since the last try); a tracker's own minimum interval does, and the status says which one holds it and until when. --force also goes past the tracker's minimum interval, at most once per tracker per 5 minutes (private trackers can penalise early announces). --wait waits up to 30 s for the tracker's answer and prints it (ok with seeders and leechers, or the plain reason it failed). Alias of `debug announce`. The announce URL is never printed, only the tracker's host.", ["seedwell reannounce sintel", "seedwell reannounce sintel --wait", "seedwell reannounce sintel --force --wait"],
        r#"{"v","transfers":[{"id","name","note","trackers":[{"url" (host only),"status","not_before","answer"}]}]}"#, "control"),
    c!("recheck", "Control", "recheck <torrent>...", "verify the files again", SEL, ["seedwell recheck sintel"], r#"{"v","action","count","transfers"}"#, "control"),
    c!("remove", "Control", "remove <torrent>... [--delete-files] [-y]", "remove torrents; asks first; --delete-files needs permission full",
        "Without -y it shows what goes and asks on the terminal. In scripts: with --json and no -y it prints the plan with a token and removes nothing; finish with `remove-confirm`.",
        ["seedwell remove sintel", "seedwell remove sintel --delete-files -y"], r#"{"v","removed":[{"id","name"}],"missing":[],"files_deleted"} (plan: {"v","token","expires_in_secs","items","summary"})"#, "control"),
    c!("remove-confirm", "Control", "remove-confirm <token>", "finish a removal started with --json and no -y", "", ["seedwell remove-confirm 0123456789abcdef0123456789abcdef"], r#"{"v","removed","missing","files_deleted"}"#, "control"),
    c!("moves", "Files on disk", "moves", "torrents that sit elsewhere than the settings say (after a location change)", "", ["seedwell moves"], r#"{"v","items":[{"id","name","bytes","from","to","kind","cross_volume"}],"total_bytes","cross_volume"}"#, "read"),
    c!("move", "Files on disk", "move <torrent>... <path> | move <torrent>... --to-settings", "relocate the files (same volume: rename; other volume: verified copy while seeding goes on)", "Needs permission full.",
        ["seedwell move sintel /Volumes/Media/Seeding", "seedwell move --all --to-settings"], r#"{"v","queued":[],"refused":[{"id","reason"}]}"#, "full"),
    c!("cancel-move", "Files on disk", "cancel-move <torrent>...", "stop queued and running moves", SEL, ["seedwell cancel-move sintel"], r#"{"v","action","count","transfers"}"#, "control"),
    c!("stream", "Files on disk", "stream <torrent> [--file N]", "a loopback URL that plays a file while it downloads (default: the largest video or audio file)", SEL, ["seedwell stream sintel --file 0"],
        r#"{"v","url","token","file_name",...}"#, "control"),
    c!("stop-stream", "Files on disk", "stop-stream <token>", "close a stream", "", ["seedwell stop-stream 0123456789abcdef"], r#"{"v","closed"}"#, "control"),
    c!("limits", "Speed and trackers", "limits [--down R] [--up R] [--alt-down R] [--alt-up R] [--alt on|off]", "global speed limits (no options: show them)", "R is a rate: 5MB/s, 500KB/s or unlimited.",
        ["seedwell limits", "seedwell limits --down 5MB/s --up 500KB/s", "seedwell limits --alt on"], r#"{"v","down_limit","up_limit","alt_active","alt_scheduled","alt_down_limit","alt_up_limit"} (null = unlimited)"#, "control"),
    c!("trackers", "Speed and trackers", "trackers", "favorite and automatically identified trackers, with live counts and caps", "", ["seedwell trackers"],
        r#"{"v","profiles":[{"id","name","favorite","private","allow_pex","transfers","sharing","up_rate","down_rate","down_limit","up_limit","urls"}],"groups":[],"public_transfers","other_private_transfers","other_private_allow_pex","pex_enabled"}"#, "read"),
    c!("tracker", "Speed and trackers", "tracker <name> limits [--down R] [--up R] | tracker <name> pex [on|off]", "the shared speed caps of one favorite tracker (no options: show them; unlimited clears), or peer exchange for a private tracker",
        "<name> is the id or name from `seedwell trackers`. `limits` is for favorites. `pex` works for any private tracker (favorite, automatic by its domain, or other-private): off by default, because most private trackers forbid peer exchange; turn it on only if the tracker's rules allow it. The master switch (`settings set network.pex=false`) overrides it. Without on/off it shows the setting; setting it needs permission full.",
        ["seedwell tracker tl limits", "seedwell tracker tl limits --down 5MB/s --up 500KB/s", "seedwell tracker tl pex", "seedwell tracker tl pex on"], r#"limits: {"v","id","down_limit","up_limit"}; pex: {"v","id","private","allow_pex","pex_enabled"}"#, "control"),
    c!("stats", "Speed and trackers", "stats [tracker] [--days N]", "upload and download history (a tracker id, public, other-private, or all)", "", ["seedwell stats", "seedwell stats tl --days 14"],
        r#"{"v","tracker","today":{"uploaded","downloaded","ratio","torrents_completed"},"this_week":{},"this_month":{},"all_time":{},"series":[{"date",...}]}"#, "read"),
    c!("rules", "Speed and trackers", "rules", "the seeding rules", "", ["seedwell rules"], r#"{"v","default":{},"rule":[{"id","name","enabled","match","goal","priority"}]}"#, "read"),
    c!("settings", "Settings", "settings get [key...] | settings set key=value...", "read or change allow-listed settings", "Keys look like transfers.download_limit. Inactive seeds: transfers.idle_min_upload_bytes (0 = off) and transfers.idle_window_hours stop a seed that uploaded less in the last hours of seeding. `set` needs permission full.",
        ["seedwell settings get transfers.download_limit", "seedwell settings set transfers.max_active_downloads=5", "seedwell settings set transfers.idle_min_upload_bytes=1000000000 transfers.idle_window_hours=150"], r#"{"v","settings":{"<key>":value},"changed":[],"note"}"#, "full"),
    c!("wait", "Watch", "wait <torrent>... [--any] [--timeout S]", "block until the torrents are done (exit 6 on timeout)", SEL, ["seedwell wait sintel --timeout 3600"], r#"{"v","done","timed_out","transfers":[...]}"#, "read"),
    c!("watch", "Watch", "watch [--kinds a,b] [--count N] [--for S]", "stream events (--json: one object per line)", "", ["seedwell watch --kinds finished,error --count 1"], r#"one object per line: {"v","seq","at","kind","id","name","data"}"#, "read"),
    c!("activity", "Watch", "activity [--limit N]", "what agents did through the control socket", "", ["seedwell activity --limit 20"], r#"{"v","entries":[{"at","client","outcome","summary"}]}"#, "full"),
    c!("log", "Watch", "log trackers [--follow] [--torrent T] [--tracker HOST] [--event E] [--errors] [--limit N]", "the developer-mode log of every tracker exchange (passkeys are masked)", "Needs Developer mode: `seedwell dev on`.",
        ["seedwell log trackers --errors --limit 20"], r#"{"v","enabled","entries":[{"ts_ms",...}]}"#, "read"),
    c!("sync", "Folder sync", "sync list | status <folder> | add <path> | join <link> [--to <path>] | share <folder> [--role R] [--uses N|unlimited] [--days N|never] | share-until <folder> <days>|never [--extend] | members <folder> | invites <folder> | revoke-invite <folder> <id> | role <folder> <device> owner|admin|read_write|read_only | remove-device <folder> <device> | rename <folder> <name> | move <folder> <path> | pause <folder> | resume <folder> | rescan <folder> | lan-only <folder> on|off | conflicts <folder> | resolve <folder> <copy> --keep original|copy|both | address <folder> <host:port> | remove <folder> [--delete-files] [-y] | settings [--device-name N] [--lan on|off] [--dht on|off] [--up R] [--down R] [--default-location PATH]",
        "keep folders the same on several devices, peer to peer (no account, no cloud)",
        "<folder> is the folder id, its first group (MFZWI3D), or the folder's name; <device> is a device id, its first group, or its name. Every device has a role in a folder: owner (changes roles, removes devices), admin (invites at read_write or read_only), read_write, read_only (receives, never sends its own changes). `add` shares a folder of this Mac (this Mac becomes its owner); `join` syncs a folder shared with you, by seedwell://sync/ invitation link, at --to or the default location (`join -` reads the link from standard input, so it stays out of the shell history); the folder starts once one of its owners or admins is online and admits this Mac. `share` makes an invitation link (owners and admins): --role (default read_write; admin and owner only from an owner), --uses (default 1; 1 to 1000 or unlimited), --days (default 7, or never: how long the link admits). `share-until` sets how long the folder is shared (owners only, permission full): <days> from now, or with --extend added to the current end, or never (no end); after the end every device but the owners is cut off (it keeps its files) and no link admits anyone, until an owner extends it; `status` and `list` show the end. Anyone with a link can join until it is used up, expires or is revoked, so `share` and `invites` need permission full and are logged (the links never are). `members` lists the folder's devices and roles; `role`, `remove-device` and `share-until` are the owner's (permission full); a removed device keeps the files it has. `rename` and `move` change the folder on this Mac only (they move files on disk: permission full). `lan-only` limits a folder to devices on the local network (on) or lifts that (off). `conflicts` lists the unresolved conflict copies (permission read); `resolve` settles one with --keep original (the file stays, the copy is archived), copy (the copy replaces the file, which is archived) or both (the copy gets a plain name); nothing is deleted. `address` adds host:port where devices of the folder can be reached. `remove` stops syncing on this Mac and asks first (like torrent removal; --json without -y prints the plan and a token for `sync remove-confirm <token>`); the files stay unless --delete-files, which moves the folder to the Trash (permission full; refused for a folder on another volume than the Trash). `settings` with no options shows them (permission read); with options it changes them (permission full); --port applies at the next start. list, status, members, conflicts and settings need permission read; share, share-until, invites, role, remove-device, rename, move, settings changes and remove --delete-files need full; the rest control.",
        ["seedwell sync list", "seedwell sync add ~/Documents/Shared", "seedwell sync share MFZWI3D", "seedwell sync share MFZWI3D --role read_only --uses 5 --days 30", "seedwell sync share-until MFZWI3D 7", "seedwell sync share-until MFZWI3D 7 --extend", "seedwell sync share-until MFZWI3D never", "seedwell sync join - --to ~/Seedwell/Shared", "seedwell sync members MFZWI3D", "seedwell sync role MFZWI3D BONSGYC admin", "seedwell sync remove-device MFZWI3D BONSGYC", "seedwell sync invites MFZWI3D", "seedwell sync revoke-invite MFZWI3D 3f2a9c01", "seedwell sync status MFZWI3D", "seedwell sync rename MFZWI3D Papers", "seedwell sync move Shared ~/Archive/Shared", "seedwell sync pause Shared", "seedwell sync resume Shared", "seedwell sync rescan Shared", "seedwell sync lan-only Shared on", "seedwell sync conflicts Shared", "seedwell sync resolve Shared 'Report (MacBook, 2026-10-25 14.32).docx' --keep both", "seedwell sync address Shared 192.168.1.20:22000", "seedwell sync remove Shared", "seedwell sync remove Shared --delete-files -y", "seedwell sync remove-confirm 0123456789abcdef0123456789abcdef", "seedwell sync settings", "seedwell sync settings --device-name 'Studio Mac' --up 2MB/s"],
        r#"list: {"v","device_id","device_name","port","external_port","folders":[{"id","name","path","access" (read_write|read_only),"role" (owner|admin|read_write|read_only, absent while joining),"share_until" (absent: no end),"state" (starting|up-to-date|scanning|syncing|waiting-for-devices|waiting-for-admin|paused|missing|error),"files","bytes","need_files","need_bytes","rate_in","rate_out","connected","members","conflicts","lan_only","issues":[{"path","reason"}],"error","last_synced_at"}],"devices":[{"id","name","client","address","via","folders","rx_bytes","tx_bytes"}]} · status/add/join/rename/move/pause/resume/rescan/lan-only/resolve/address/role/remove-device/revoke-invite/share-until: one folder as in list · share: {"v","id","link","role","max_uses" (0 = unlimited),"uses","expires_at","created_at","revoked"} · invites: {"v","id","name","invites":[as share]} · members: {"v","id","name","members":[{"device_id","name","role","online","me","expires_at" (the folder's share end, for devices that are not owners)}]} · conflicts: {"v","id","name","conflicts":[{"copy","original","device","when"}]} · remove: {"v","token","expires_in_secs","delete_files","folder","summary"} then {"v","id","name","path","files" (kept|trashed),"trashed_to"} · settings: {"v","device_name","port","lan_discovery","dht_discovery","relay_own_devices","relay_everyone","up_limit","down_limit","default_location","changed","note"}"#, "control"),
    c!("relay", "Folder sync", "relay [--port N] [--bind ADDR] [--key-file PATH]", "run a self-hosted relay for folder sync (it carries only end-to-end encrypted bytes)",
        "For devices that cannot reach each other directly. Runs in the foreground until Ctrl-C and needs no app or control socket. Port 22067 unless --port. The relay's key (its id is part of its address) is kept in --key-file, by default ~/Library/Application Support/Seedwell/relay.key (macOS) or ~/.local/state/seedwell/relay.key.",
        ["seedwell relay", "seedwell relay --port 22067 --key-file /srv/seedwell/relay.key"], "(prints one line: seedwell relay ready address=relay://<ip:port>#<relay id> pid=<pid>)", "none"),
    c!("debug", "Debug", "debug health | trackers [<torrent>] [--tracker X] | trackers log [...] | announce <torrent> | peers <torrent> | perf",
        "diagnose trackers, peers, connections and performance (read-only except `announce`)",
        "health: is anything wrong, with the command to dig deeper. trackers: state, timing, failures and the announce gate per tracker. trackers log: the developer log (--follow, --since 10m; it records only while Developer mode is on: `seedwell dev on`). announce: Ask Tracker Again (flags --force and --wait as in `reannounce`). peers: flags, rates, requests in flight (--redact masks the last address part). perf: rates, limits, gates, port mapping, DHT, process. Add --watch (-f) to health, trackers, peers and perf to refresh every second.",
        ["seedwell debug health", "seedwell debug trackers sintel", "seedwell debug trackers --tracker tl", "seedwell debug trackers log --follow --errors --since 10m", "seedwell debug announce sintel", "seedwell debug peers sintel --watch", "seedwell debug perf --json"],
        r#"health: {"v","ok","summary","issues":[{"area","severity","message","next"}]} · trackers: {"v","online","offline_since","transfers":[{"id","name","state","trackers":[{"url","tier","state","status","failures","last_announce_at","last_success_at","seeders","leechers","next_announce_at","next_reason","manual_ok_at","tripped"}]}],"truncated"} · peers: {"v","id","name","peers":[{"addr","client","transport","direction","choking_us","choked_by_us","interested","down_rate","up_rate","requests_in_flight","progress","connected_secs","snubbed","upload_only","has_pex"}],"truncated","connected","half_open","max_connections","incoming_accepted","incoming_refused"} · perf: {"v","totals","limits","probe":{...},"process":{"pid","rss_bytes","cpu_percent","threads","open_fds"},"torrents":[...],"torrent_count","peers"}"#, "read"),
    c!("dev", "Debug", "dev on|off|status", "Developer mode (Settings > Advanced): logs every tracker exchange and is what `sim` requires",
        "`on` and `off` need permission full and apply live, like the Settings switch. `status` shows whether it is on and the tracker log's path and size. This is the one way to switch it from the terminal.",
        ["seedwell dev status", "seedwell dev on", "seedwell dev off"], r#"{"v","developer_mode","log_path","log_bytes"}"#, "full"),
    c!("sim", "Test", "sim add <kind> [options] | list | stop [<sim>] | speed <sim> R | tracker <sim> ok|unregistered|down|slow|fail503 | clear | scenario <name>",
        "simulated torrents with fake data and a loopback swarm, for testing (needs Developer mode or --yes-i-know)",
        "Kinds: movie season-pack album audiobook book software archive single magnet. Options: --size 1.5GB --speed 2MB/s --peers N --private --tracker ok|unregistered|down|slow|fail503|interval=60 --stall-after 50% --corrupt N --extras --name TEXT --seasons N --episodes N --files N --hold. Everything binds to 127.0.0.1 and the data lives in the sim folder. Scenarios: redownload outage storm-300 private-unregistered.",
        ["seedwell sim add season-pack --seasons 2 --episodes 8 --speed 2MB/s", "seedwell sim add movie --size 1.5GB --tracker unregistered --hold", "seedwell sim speed sim1 500KB/s", "seedwell sim tracker sim1 down", "seedwell sim stop sim1", "seedwell sim scenario redownload", "seedwell sim clear"],
        r#"{"v","sims":[{"id","kind","name","torrent_id","speed","tracker","peers","running"}]} (add: {"v","sim","torrent_id","name","added"})"#, "full"),
    c!("skill", "Agents", "skill show | install [--dir PATH] [--force] | uninstall [--dir PATH]", "the Agent Skill for Claude Code that ships inside the program", "install writes SKILL.md to ~/.claude/skills/seedwell/ (only when you run it) and asks before replacing a changed file unless --force.",
        ["seedwell skill show", "seedwell skill install", "seedwell skill install --dir ./skills/seedwell --force", "seedwell skill uninstall"], r#"{"v","action","path","files":[]}"#, "none"),
    c!("help", "Agents", "help [command | agents]", "this overview, one command's help, or the compact guide for AI agents", "", ["seedwell help", "seedwell help agents", "seedwell help pause"], "(text)", "none"),
    c!("daemon", "Test", "daemon --state-dir D --download-dir X [--listen-port P] [--ask] [--grace S] [--allow-lan-trackers] [--dev-mode] [--no-sync]",
        "a headless engine with its own control socket, for tests and CI", "Run the other commands against it with --socket D/control.sock.", [], "(prints one `seedwell daemon ready socket=… port=… pid=…` line)", "none"),
];

/// Look a command up by name.
pub fn find(name: &str) -> Option<&'static CmdHelp> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// `seedwell help`.
pub fn overview() -> String {
    let mut o = String::from(
        "seedwell - control Seedwell (the BitTorrent client and download manager) from the terminal\n\n\
USAGE\n  seedwell [--json] [--socket PATH] [--launch] <command> [options]\n\n",
    );
    let mut last = "";
    for c in COMMANDS {
        if c.group != last {
            o.push_str(&format!("{}\n", c.group.to_uppercase()));
            last = c.group;
        }
        o.push_str(&format!("  {:<15} {}\n", c.name, c.summary));
    }
    o.push_str(
        "\nGLOBAL OPTIONS\n  --json          machine-readable output; every object has \"v\":1\n  \
--socket PATH   control socket (default: $SEEDWELL_SOCKET or ~/Library/Application Support/Seedwell/control.sock)\n  \
--launch        start the Seedwell app in the background when it is not running\n  \
--client NAME   name shown in the activity log\n  -h, --help      help for a command: seedwell <command> --help\n  -V, --version\n\n",
    );
    o.push_str("TORRENTS\n  ");
    o.push_str(SEL);
    o.push_str("\n\nUNITS\n");
    for l in UNITS.lines() {
        o.push_str(&format!("  {l}\n"));
    }
    o.push_str("\nEXIT CODES\n");
    for l in EXIT_CODES.lines() {
        o.push_str(&format!("  {l}\n"));
    }
    o.push_str("\n`seedwell help agents` is a compact guide for AI agents. `seedwell skill install` adds it to Claude Code.\n");
    o
}

/// `seedwell help <command>` / `seedwell <command> --help`.
pub fn command_help(c: &CmdHelp) -> String {
    let mut o = format!("seedwell {}\n\n  {}\n\nUSAGE\n  seedwell {}\n", c.name, c.summary, c.usage);
    if !c.details.is_empty() {
        o.push_str(&format!("\n{}\n", c.details));
    }
    if !c.examples.is_empty() {
        o.push_str("\nEXAMPLES\n");
        for e in c.examples {
            o.push_str(&format!("  {e}\n"));
        }
    }
    o.push_str(&format!("\nJSON\n  {}\n", c.json));
    if c.needs != "none" {
        o.push_str(&format!("\nNEEDS\n  permission {} (Settings > Agents)\n", c.needs));
    }
    o.push_str("\nExit codes: 0 ok, 1 error, 2 usage, 3 not found/ambiguous, 4 permission/agent access off, 5 app not running, 6 timed out.\n");
    o
}

/// The compact guide `seedwell help agents` prints.
pub fn agents_guide() -> String {
    let mut o = String::new();
    o.push_str(AGENTS_HEAD);
    o.push_str("\nCOMMANDS (add --json to any; `seedwell <command> --help` has examples)\n");
    for c in COMMANDS {
        o.push_str(&format!("  seedwell {}\n      {}   [needs: {}]\n", c.usage.lines().next().unwrap_or(""), c.summary, c.needs));
    }
    o.push_str("\nUNITS\n");
    for l in UNITS.lines() {
        o.push_str(&format!("  {l}\n"));
    }
    o.push_str("\nEXIT CODES\n");
    for l in EXIT_CODES.lines() {
        o.push_str(&format!("  {l}\n"));
    }
    o.push_str(AGENTS_RECIPES);
    o
}

/// Opening of the agents guide (also the body of the skill's first section).
pub const AGENTS_HEAD: &str = "\
SEEDWELL FOR AI AGENTS

Seedwell is a macOS BitTorrent client. `seedwell` talks to the running app over a local socket; the user decides
what you may do in Settings > Agents (off / read-only / control / full). Exit 4 means you lack permission or access is off:
tell the user, do not retry. Exit 5 means the app is not running: ask the user to open it (or pass --launch).

RULES OF THUMB
- Add --json and read the object. Every object has \"v\":1; fields are only ever added. Errors are
  {\"v\":1,\"error\":{\"code\",\"message\",\"exit\",\"candidates\"?}} on stderr.
- <torrent> is an id prefix (6+ hex characters from `list`), part of the name, or a filter (--all, --tracker X,
  --state S, --category C). An ambiguous name is exit 3 with a candidate list: pick an id from it.
- Rates and sizes: 5MB/s, 500KB/s, 1.5GB, unlimited. JSON carries bytes and bytes/s.
- New torrents are held: confirm with `seedwell start <torrent>`, dismiss with `seedwell cancel <torrent>`
  (the .torrent file goes to the Trash; --keep-file keeps it).
- Never delete files unless the user asked: `remove` keeps the data unless --delete-files (needs permission full).
- Tracker passkeys never appear in any output.
- Developer mode (`seedwell dev on|off|status`, permission full) turns on the tracker log and is required by `sim`.
- Folder sync links (seedwell://sync/...) are invitations: anyone with one can join the folder until it is used up,
  expires or is revoked. Only `seedwell sync share` and `seedwell sync invites` print them (permission full); never
  paste them into logs, issues or chat unless the user asks.";

/// Recipes at the end of the agents guide and in the skill.
pub const AGENTS_RECIPES: &str = "
RECIPES
Why isn't my torrent downloading?
  seedwell debug health                 anything wrong at all? each finding names the next command
  seedwell debug trackers <torrent>     is the tracker answering? halted (unregistered), backing off, offline-held?
  seedwell debug peers <torrent>        any peers? who chokes us? requests in flight? snubbed?
  seedwell debug perf                   limits throttling? connection budget full? port mapped? memory?
  seedwell debug announce <torrent>     Ask Tracker Again (says what holds it back, if anything; --force, --wait)
Watch something happen:  seedwell debug peers <torrent> --watch   |   seedwell debug trackers log --follow --errors
Wait for a download:     seedwell wait <torrent> --timeout 3600     (exit 6 on timeout)
Add and confirm:         seedwell add file.torrent --json   then   seedwell start <id>   (or: seedwell cancel <id>)
Choose files:            seedwell choice <torrent>   then   seedwell start <torrent> --files 0-9,12
Cap the speed:           seedwell limits --down 5MB/s --up 500KB/s     |   seedwell tracker <favorite> limits --up 1MB/s
Share a folder:          seedwell sync add ~/Documents/Shared   then   seedwell sync share Shared   (give the key to the other device)
Join a folder:           seedwell sync join - --to ~/Seedwell/Shared   (paste the key on standard input)
Is a folder in sync?     seedwell sync status Shared   |   seedwell sync list

TESTING WITH SIMULATED TORRENTS (needs Developer mode: `seedwell dev on`, or --yes-i-know)
  seedwell dev on                                                        once; `seedwell dev status` shows the tracker log
  seedwell sim add season-pack --seasons 2 --episodes 8 --speed 2MB/s     fake data, a loopback seeder and tracker
  seedwell sim add movie --size 1.5GB --tracker unregistered              a halted tracker: test \"Ask Again\"
  seedwell sim speed <sim> 500KB/s | seedwell sim stop <sim> | seedwell sim tracker <sim> down
  seedwell sim scenario redownload | outage | storm-300 | private-unregistered
  seedwell sim clear                                                     removes every sim torrent and its data
";
