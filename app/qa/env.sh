#!/usr/bin/env bash
# Seedwell QA environment helpers. Everything lives under one temp root; nothing touches the real
# home folder or real state. `source env.sh`, then call the functions.
#
# The app under test is quiet (QuietRun, started by --qa-control): it never becomes the active or frontmost app,
# never takes the focus or a key, shows no window on a screen and has no menu bar item or Dock icon. `qa_shot`
# draws its windows in-process, so nothing here needs an awake display or the Screen Recording permission.
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QA_ROOT="${QA_ROOT:-/private/tmp/claude-501/seedwell-qa}"
HOMEROOT="$QA_ROOT/home"        # --home-root (the app's `~`)
STATE="$QA_ROOT/state"          # --state-dir
SWARM="$QA_ROOT/swarm"          # generated data + torrents
APP="$REPO/app/build/Seedwell.app"
APPBIN="$APP/Contents/MacOS/SeedwellApp"
CLI="$APP/Contents/MacOS/seedwell"
SOCK="$STATE/control.sock"
CTL="$QA_ROOT/qa-control"
REPORT="$CTL.bench"
OUT="${QA_OUT:-$REPO/app/build/qa}"
TRS="${TRS_DAEMON:-$REPO/target/release/seedwell}"   # a seedwell binary with the engine (the app bundle's CLI is client-only)
FAIL=0

# Start a long-lived process in its own session so it outlives the calling shell.
detach() { python3 -c 'import os,sys; os.setsid(); os.execvp(sys.argv[1], sys.argv[1:])' "$@"; }

qa_guard() { case "$QA_ROOT" in /private/tmp/*|/tmp/*|/var/folders/*) ;; *) echo "QA_ROOT must be a temp dir"; exit 1;; esac; }

# qa_until SECS COMMAND...: until COMMAND succeeds (checked every 0.2 s), for what only the outside sees (the CLI,
# files, ports); fails after SECS.
qa_until() {
  local end=$((SECONDS + $1)); shift
  until "$@" >/dev/null 2>&1; do [ $SECONDS -lt "$end" ] || return 1; sleep 0.2; done
}

# qa_listening PORT...: every port has a listener.
qa_listening() { local p; for p in "$@"; do lsof -nP -iTCP:"$p" -sTCP:LISTEN -t >/dev/null 2>&1 || return 1; done; }

# The trackers keep running after the script: their output goes to a file, not the caller's stdout (an open pipe
# kept `scenario.sh | tail` waiting for them).
qa_trackers_start() {
  mkdir -p "$QA_ROOT/trk"
  detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7101 --mode ok --log "$QA_ROOT/trk/7101.log" >"$QA_ROOT/trk/7101.out" 2>&1 &
  detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7102 --mode ok --log "$QA_ROOT/trk/7102.log" >"$QA_ROOT/trk/7102.out" 2>&1 &
  detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7103 --mode unregistered --log "$QA_ROOT/trk/7103.log" >"$QA_ROOT/trk/7103.out" 2>&1 &
  detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7104 --mode down --log "$QA_ROOT/trk/7104.log" >"$QA_ROOT/trk/7104.out" 2>&1 &
  qa_until 10 qa_listening 7101 7102 7103 7104
}
qa_trackers_stop() { local pid; for pid in $(pgrep -f "qa_tracker.py .*$QA_ROOT/" 2>/dev/null); do kill "$pid" 2>/dev/null; done; true; }

# qa_port_free PORT: a fixed-port seeder needs its port. Another QA scene left running (each script tears down only
# its own QA root) would keep it, the new seeder would fall back to a random port, and the app would download from
# the old scene's seeder instead. Waits a few seconds for a seeder that is just quitting, then refuses.
qa_port_free() {
  local pid
  for i in $(seq 1 20); do pid=$(lsof -nP -iTCP:"$1" -sTCP:LISTEN -t 2>/dev/null | head -1); [ -z "$pid" ] && return 0; sleep 0.25; done
  echo "port $1 is taken by pid $pid: $(ps -o command= -p "$pid" | cut -c1-200)"
  echo "another QA scene is still running; tear it down first (QA_ROOT=<its root> app/qa/teardown.sh)"
  return 1
}

# qa_seeder PORT DATA-DIR UPLOAD-LIMIT [DIR]: a headless, rate-limited seedwell daemon on 127.0.0.1:PORT whose
# downloads go to DATA-DIR; $SEEDER is its folder (DIR, default $QA_ROOT/seeder), "$SEEDER/control.sock" its socket.
qa_seeder() {
  SEEDER="${4:-$QA_ROOT/seeder}"
  mkdir -p "$SEEDER"
  printf 'version = 1\n[transfers]\nupload_limit = %s\n' "$3" > "$SEEDER/config.toml"
  qa_port_free "$1" || return 1
  TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/seeder-home" detach "$TRS" daemon --state-dir "$SEEDER" --download-dir "$2" --listen-port "$1" \
    --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$SEEDER/control.sock" >"$SEEDER.log" 2>&1 &
  qa_until 10 test -S "$SEEDER/control.sock"
}
# qa_seed SAVE-PATH TORRENT...: the seeder of qa_seeder seeds them, their data in SAVE-PATH.
qa_seed() { local d="$1" t; shift; for t in "$@"; do "$TRS" --socket "$SEEDER/control.sock" add "$t" --save-path "$d" >/dev/null || echo "seeder add failed: $t"; done; }

# Config for the app under test: every folder under the isolated home, no router, no DHT; a watch folder only with
# QA_WATCH (its files stay where they are).
qa_write_config() {
  mkdir -p "$STATE" "$HOMEROOT/Downloads"
  cat > "$STATE/config.toml" <<TOML
version = 1
[agents]
permission = "full"
notify_changes = false
[network]
upnp = false
natpmp = false
dht = false
pex = false
lsd = false
listen_port = 0
[transfers]
ask_before_adding = "never"
max_active_downloads = 3
start_delay_secs = ${QA_START_DELAY:-0}
[advanced]
move_limit_mib = 1
[folders]
download_dir = "~/Downloads"
incomplete_dir = "~/Downloads/.incomplete"
torrent_after_add = "leave"
TOML
  # No watch folder unless asked for (QA_WATCH="~/Watch"): without one the app adds ~/Downloads, which puts the
  # .torrent files it takes in the system's Trash (the user's real one, also in an isolated run). Never "trash" here.
  if [ -n "${QA_WATCH:-}" ]; then
    printf '\n[[folders.watch]]\npath = "%s"\nmode = "auto"\nafter_add = "leave"\n' "$QA_WATCH" >> "$STATE/config.toml"
  else
    echo 'watch = []' >> "$STATE/config.toml"
  fi
}

# qa_setting KEY=VALUE...: changes settings open to agents through the CLI (`seedwell settings set`, which writes
# config.toml); qa_set_config KEY VALUE: a line of config.toml as a hand edit (`start_delay_secs 9`,
# `ask_before_adding '"multi_file"'`). Both return once the engine has read the file again and applied it.
qa_reloads() { qa_ask "config" | grep '^config:' | tail -1; }
qa_setting() {
  local n; n=$(qa_reloads)
  "$CLI" --socket "$SOCK" settings set "$@" >/dev/null || { echo "settings set failed: $*"; FAIL=1; return 1; }
  qa_wait "config !~ $n" 15 >/dev/null || { echo "the app did not apply: $*"; FAIL=1; return 1; }
}
qa_set_config() {
  local n; n=$(qa_reloads)
  python3 -c '
import re, sys
p, k, v = sys.argv[1:4]
s = open(p).read()
s, n = re.subn(rf"(?m)^{k} = .*$", f"{k} = {v}", s)
assert n == 1, f"{k} is not in {p}"
open(p, "w").write(s)' "$STATE/config.toml" "$1" "$2" || { FAIL=1; return 1; }
  qa_wait "config !~ $n" 15 >/dev/null || { echo "the app did not apply $1 = $2"; FAIL=1; return 1; }
}

qa_pid() { pgrep -f "SeedwellApp --state-dir $STATE" | head -1; }
qa_running() { pgrep -f "SeedwellApp --state-dir $STATE" >/dev/null; }
qa_stopped() { ! qa_running; }
qa_ready() { [ -S "$SOCK" ] && [ -f "$CTL.ack" ]; }

# qa_app_start [app args...]: the app, started in the background (`open -g`: never frontmost), ready for QA commands
# when this returns. Environment for the app: QA_APP_ENV="NAME=value" (e.g. SEEDWELL_QA_TRASH_DIR).
qa_app_start() {
  qa_guard
  # The engine moves a .torrent file it added to the system's Trash when told to ("trash", also the default): in an
  # isolated run that is still the user's real Trash. QA configs say "leave" (Cancel's Trash is SEEDWELL_QA_TRASH_DIR).
  if [ ! -f "$STATE/config.toml" ] || grep -qE '^ *(torrent_)?after_add *= *"trash"' "$STATE/config.toml" \
     || ! grep -qE '^ *torrent_after_add *= *"' "$STATE/config.toml"; then
    echo "refusing to start: $STATE/config.toml would let the app move .torrent files to the real Trash"; FAIL=1; return 1
  fi
  : > "$CTL"; rm -f "$CTL.ack"
  open -g -n ${QA_APP_ENV:+--env "$QA_APP_ENV"} "$APP" --args --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL" --qa-loopback-trackers "$@"
  # The control socket (the core is up) and the ack file (QA commands are read).
  qa_until 60 qa_ready || { echo "the app did not start (QA_ROOT=$QA_ROOT)"; FAIL=1; return 1; }
}

# qa_app_quit: Quit as the menu's Quit row does; waits until the process has ended (it lingers until the trackers
# heard `stopped`), killing it after 30 s.
qa_app_quit() {
  qa_running || return 0
  printf 'quit\n' >> "$CTL"
  qa_until 30 qa_stopped && return 0
  local p; p=$(qa_pid); [ -n "$p" ] && kill "$p" 2>/dev/null
  qa_until 10 qa_stopped
  return 0
}
qa_app_stop() { qa_app_quit; }

# qa_cmd "section sharing" ... : appends commands; returns once the app has done them and laid its windows out
# (`<file>.ack`), so a shot or a report right after sees them. Fails when the app is gone or gives no answer in
# QA_CMD_TIMEOUT seconds (120).
qa_cmd() {
  local before; before=$(wc -l < "$CTL" | tr -d ' ')
  local c; for c in "$@"; do printf '%s\n' "$c" >> "$CTL"; done
  local want=$((before + $#)) end=$((SECONDS + ${QA_CMD_TIMEOUT:-120})) n=0
  while [ "$(cat "$CTL.ack" 2>/dev/null)" != "$want" ]; do
    n=$((n + 1))
    if [ $((n % 50)) = 0 ]; then
      qa_running || { echo "qa_cmd: the app is not running ($*)" >&2; return 1; }
      [ $SECONDS -lt $end ] || { echo "qa_cmd: no answer in ${QA_CMD_TIMEOUT:-120} s to: $*" >&2; return 1; }
    fi
    sleep 0.02
  done
}

# qa_ask COMMAND...: runs them and prints what they reported (their new lines of `<file>.bench`).
qa_ask() {
  local n; n=$(cat "$REPORT" 2>/dev/null | wc -l | tr -d ' ')
  qa_cmd "$@" || return 1
  tail -n +"$(( ${n:-0} + 1 ))" "$REPORT" 2>/dev/null
}

# qa_wait "COMMAND ~ TEXT" [SECS]: answered by the app once COMMAND's report holds TEXT (`!~`: no longer holds it),
# default within 15 s; prints that report line, or fails with "wait: timeout after …".
#   qa_wait "where ~ section downloads" 10;  qa_wait "windows ~ chooser";  qa_wait "starting state ~ shown true"
qa_wait() {
  local out; out=$(qa_ask "wait $1${2:+ timeout $2}" | tail -1)
  echo "$out"
  case "$out" in "wait: timeout"*|"wait: <command>"*|"") return 1;; esac
}

# qa_shot NAME [WHAT]: draws WHAT (main, settings, info, chooser, popover, menu, alert, window <id>; see QAShots) at
# 2x into $OUT/NAME.png, in-process. A missing shot is a failure (FAIL=1).
qa_shot() {
  mkdir -p "$OUT"
  local out; out=$(qa_ask "shot $OUT/$1.png ${2:-main}" | grep '^shot: ' | tail -1)
  case "$out" in
    *" FAILED: "*|"") echo "  MISSING $1 (${out##* FAILED: })"; FAIL=1; [ -n "${QA_SHOT_LOG:-}" ] && echo "MISSING $1" >> "$QA_SHOT_LOG"; return 1;;
    "shot: $OUT/$1.png "*) echo "  $1"; [ -n "${QA_SHOT_LOG:-}" ] && echo "$1" >> "$QA_SHOT_LOG"; return 0;;
    *) echo "  MISSING $1 ($out)"; FAIL=1; [ -n "${QA_SHOT_LOG:-}" ] && echo "MISSING $1" >> "$QA_SHOT_LOG"; return 1;;
  esac
}

# check LABEL OUTPUT WANTED: OUTPUT holds WANTED. lacks LABEL OUTPUT UNWANTED: it does not. A miss sets FAIL=1.
check() { case "$2" in *"$3"*) echo "  ok: $1";; *) echo "  FAIL: $1: wanted '$3' in: $2"; FAIL=1;; esac; }
lacks() { case "$2" in *"$3"*) echo "  FAIL: $1: did not want '$3' in: $2"; FAIL=1;; *) echo "  ok: $1";; esac; }

# qa_teardown: everything of this QA root (the app, seeders, leechers, trackers, servers: every process whose
# command line names the root) and the root's disk images. Other roots are never touched. The data stays.
qa_teardown() {
  qa_app_quit
  local pid; for pid in $(pgrep -f "$QA_ROOT/" 2>/dev/null); do [ "$pid" != "$$" ] && kill "$pid" 2>/dev/null; done
  local v; for v in "$QA_ROOT"/vol*; do [ -d "$v" ] && mount | grep -q " on $v " && hdiutil detach "$v" -force >/dev/null 2>&1; done
  true
}

# qa_fresh: a clean root for this script (whatever ran in it before is torn down first).
qa_fresh() { qa_guard; qa_teardown; rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"; }

# qa_end NAME: tears the root down and ends the script with its verdict (exit 0 PASS, 1 FAIL), as run.sh reads it.
qa_end() {
  qa_teardown
  if [ "$FAIL" = 0 ]; then echo "$1: PASS"; exit 0; else echo "$1: FAIL"; exit 1; fi
}

# Add generated torrents through the app's own control socket (permission full in config.toml).
qa_add_all() { # dir-of-torrents data-dir
  for t in "$1"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" --save-path "$2" >/dev/null 2>&1 || echo "add failed: $t"; done
}
# qa_count [FILTER]: how many torrents `seedwell list [FILTER]` shows.
qa_count() { "$CLI" --socket "$SOCK" list ${1:+"$1"} --json --limit 1000 2>/dev/null | python3 -c "import json,sys;print(len(json.load(sys.stdin)['transfers']))" 2>/dev/null || echo 0; }
# qa_listed FILTER TEXT: a torrent whose line holds TEXT is listed under FILTER (`qa_until 60 qa_listed recent Bunny`).
qa_listed() { "$CLI" --socket "$SOCK" list "$1" 2>/dev/null | grep -q "$2"; }
# qa_scene: the standard scene (scenario.sh) in this root, built unless it already runs. run.sh keeps it for the
# scripts that share it and tears it down after the last of them; by hand: teardown.sh.
qa_scene() { qa_running && [ -f "$QA_ROOT/scene-ready" ] && return 0; bash "$REPO/app/qa/scenario.sh" "$@" >"$QA_ROOT.scene.log" 2>&1 || { echo "scenario.sh failed:"; tail -5 "$QA_ROOT.scene.log"; return 1; }; }
