#!/usr/bin/env bash
# Developer mode (SPEC 8.7): a private-style local tracker (the passkey is in the announce URL), the
# real app with developer mode on, Settings > Advanced and the tracker log viewer with entries, dark
# and light, into app/build/qa/devlog-*.png. Then the proof: the real passkey string is looked for in
# the log files and the script fails when it is there. Own QA root (seedwell-qa-dev).
#
# Kills only what it started, by PID (the app only through its own --state-dir under the QA root).
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-dev"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
PASSKEY="0f3a9c52e7b14d68a1c0d9e5f2b7a346"       # the secret the log must never contain
PIDS="$QA_ROOT/pids"

stop_all() {
  # the app: by the QA state dir only; the helpers: by the PIDs recorded below
  for pid in $(pgrep -f "SeedwellApp --state-dir $STATE" 2>/dev/null); do kill "$pid" 2>/dev/null || true; done
  [ -f "$PIDS" ] && for pid in $(cat "$PIDS"); do kill "$pid" 2>/dev/null || true; done
  : > "$PIDS" 2>/dev/null || true
}
trap 'stop_all' EXIT

stop_all
sleep 1
rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"; : > "$PIDS"

# --- a private-style tracker world: ok, unregistered (failure reason), down (503) -----------------------------
for spec in "7301 ok" "7302 unregistered" "7303 down"; do
  set -- $spec
  python3 -I "$REPO/app/qa/qa_tracker.py" --port "$1" --mode "$2" --interval 1800 --log "$QA_ROOT/tracker-$1.log" &
  echo $! >> "$PIDS"
done
sleep 0.7
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/swarm" --count 6 --seed 11 \
  --trackers "ok=http://127.0.0.1:7301/announce?passkey=$PASSKEY,unreg=http://127.0.0.1:7302/$PASSKEY/announce,down=http://127.0.0.1:7303/announce?authkey=$PASSKEY&torrent_pass=$PASSKEY" >/dev/null

# --- the app: developer mode on in config.toml ------------------------------------------------------------------
qa_write_config
python3 - "$STATE/config.toml" <<'PY'
import sys, re
p = sys.argv[1]
s = open(p).read()
s = s.replace("[advanced]\nmove_limit_mib = 1\n", "[advanced]\nmove_limit_mib = 1\ndeveloper_mode = true\n")
open(p, "w").write(s)
PY
caffeinate -d -u -t 3600 >/dev/null 2>&1 &
echo $! >> "$PIDS"
: > "$CTL"
open -n "$APP" --args --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL" --qa-loopback-trackers --window-size 1180x820 --show-window
for i in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.5; done
qa_add_all "$QA_ROOT/swarm/torrents" "$QA_ROOT/swarm/data"
echo "waiting for the announces (the gate lets one request per second through to each host) ..."
sleep 25

shot_window() {   # name: the main window, with the sheet when one is attached
  qa_cmd "activate main" >/dev/null
  QA_PID=$(pgrep -f "SeedwellApp --state-dir $STATE" | head -1); export QA_PID
  local ids; ids=$("$OUT/winid" Seedwell | awk '{print $1}')
  local first=1 id
  for id in $("$OUT/winid" Seedwell | awk '$2==0 {print $1" "$4}' | sort -t' ' -k2 | cut -d' ' -f1); do
    screencapture -x -o -l "$id" "$OUT/devlog-$1-w$id.png" 2>/dev/null
  done
  # keep the largest window capture as the named shot
  local best; best=$(ls -S "$OUT"/devlog-$1-w*.png 2>/dev/null | head -1)
  [ -n "$best" ] && mv "$best" "$OUT/devlog-$1.png" && rm -f "$OUT"/devlog-$1-w*.png && echo "  devlog-$1"
}

rm -f "$CTL.bench"
for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "settings advanced"
  sleep 1.5; shot_window "settings-$mode"
  qa_cmd "devlog open" "devlog expand none"
  sleep 2.5; shot_window "viewer-$mode"
  qa_cmd "devlog expand last"
  sleep 1.2; shot_window "viewer-expanded-$mode"
  qa_cmd "devlog errors on" "devlog expand first"
  sleep 2.2; shot_window "viewer-errors-$mode"
  qa_cmd "devlog errors off" "devlog close"
done
qa_cmd "devlog state"; sleep 1.5
echo "viewer state:"; tail -1 "$CTL.bench" 2>/dev/null

# --- the proof ------------------------------------------------------------------------------------------------------
LOGS="$STATE/logs"
echo "== $LOGS"
ls -l "$LOGS" 2>/dev/null
LINES=$(cat "$LOGS"/trackers.log* 2>/dev/null | wc -l | tr -d ' ')
echo "log lines: $LINES"
if [ "$LINES" -lt 6 ]; then echo "FAIL: the log is nearly empty"; exit 1; fi
if grep -rq "$PASSKEY" "$LOGS"; then echo "FAIL: the real passkey is in the log:"; grep -rn "$PASSKEY" "$LOGS" | head -3; exit 1; fi
echo "OK: grep for the real passkey ($PASSKEY) in the log files finds nothing"
echo "    but the trackers did receive it (so the test is meaningful): $(cat "$QA_ROOT"/tracker-730*.log | grep -c "$PASSKEY") request line(s) in their own logs"
grep -rh '"kind":"announce"' "$LOGS" | head -2 | cut -c1-400
# the client key is masked too
KEYS=$(grep -rhoE '"key":"[^"]*"' "$LOGS" | sort | uniq -c)
echo "key field values: $KEYS"
