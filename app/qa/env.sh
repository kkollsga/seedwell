#!/usr/bin/env bash
# Seedwell QA environment helpers. Everything lives under one temp root; nothing touches the real
# home folder or real state. `source env.sh`, then call the functions.
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
OUT="$REPO/app/build/qa"

# Start a long-lived process in its own session so it outlives the calling shell.
detach() { python3 -c 'import os,sys; os.setsid(); os.execvp(sys.argv[1], sys.argv[1:])' "$@"; }

qa_guard() { case "$QA_ROOT" in /private/tmp/*|/tmp/*|/var/folders/*) ;; *) echo "QA_ROOT must be a temp dir"; exit 1;; esac; }

qa_trackers_start() {
  mkdir -p "$QA_ROOT/trk"
  detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7101 --mode ok --log "$QA_ROOT/trk/7101.log" &
  detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7102 --mode ok --log "$QA_ROOT/trk/7102.log" &
  detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7103 --mode unregistered --log "$QA_ROOT/trk/7103.log" &
  detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7104 --mode down --log "$QA_ROOT/trk/7104.log" &
  sleep 0.5
}
qa_trackers_stop() { pkill -f "qa_tracker.py --port" 2>/dev/null; true; }

# Config for the app under test: every folder under the isolated home, no router, no DHT.
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
}

# A sleeping display makes macOS refuse to activate any app (windows draw in the grey inactive
# state) and makes `screencapture -l` fail: keep it awake for the run (default one hour).
qa_wake() {
  pkill -f "caffeinate -d -u -t" 2>/dev/null || true
  detach caffeinate -d -u -t "${1:-3600}" >/dev/null 2>&1 &
  sleep 0.5
}

qa_app_start() {
  qa_guard
  qa_wake
  : > "$CTL"
  # Through LaunchServices, so the app starts frontmost and its windows look active (accent colours).
  open -n "$APP" --args --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL" --qa-loopback-trackers "$@"
  for i in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.5; done
}
qa_app_stop() { pkill -f "SeedwellApp --state-dir $STATE" 2>/dev/null; true; }

# qa_cmd "section sharing" ... : append commands, wait for the app to acknowledge them
qa_cmd() {
  # winid must only see this QA app's windows, never the user's real Seedwell.
  QA_PID=$(pgrep -f "SeedwellApp --state-dir $STATE" | head -1); export QA_PID
  local before; before=$(wc -l < "$CTL" | tr -d ' ')
  for c in "$@"; do echo "$c" >> "$CTL"; done
  local want=$((before + $#))
  for i in $(seq 1 40); do [ "$(cat "$CTL.ack" 2>/dev/null)" = "$want" ] && break; sleep 0.15; done
  sleep 0.8
}

# qa_shot name [owner]: capture the app's main window (largest non-menu window) with screencapture -l
qa_shot() {
  mkdir -p "$OUT"
  qa_cmd "activate" >/dev/null
  local id; id=$("$OUT/winid" Seedwell | awk '$2==0 {print $1" "$4}' | sort -t' ' -k2 | tail -1 | cut -d' ' -f1)
  screencapture -x -o -l "$id" "$OUT/$1.png" && echo "captured $1 (window $id)"
}
# qa_shot_window name pattern: capture the window whose title contains pattern
qa_shot_title() {
  mkdir -p "$OUT"
  local id; id=$("$OUT/winid" Seedwell | grep -i "$2" | head -1 | cut -d' ' -f1)
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "captured $1 (window $id)"
}

# Add generated torrents through the app's own control socket (permission full in config.toml).
qa_add_all() { # dir-of-torrents data-dir
  for t in "$1"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" --save-path "$2" >/dev/null 2>&1 || echo "add failed: $t"; done
}
