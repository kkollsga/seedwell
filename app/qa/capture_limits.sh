#!/usr/bin/env bash
# Speed limits (SPEC 7, 7E.4, 8.3; own QA root seedwell-qa-lim): Settings > Transfers with the number +
# unit controls (MB/s and KB/s values, empty = Unlimited), a favorite tracker's Info page with shared
# caps and its current rate against them, the same tracker before it is a favorite ("Save as Favorite to
# set limits"), and the main window's speed line with the limit marker ("↓ 2.4 / 5 MB/s"). A throttled
# headless seeder feeds three downloads. Dark and light, into app/build/qa/limits-*.png.
#
# Kills only what it started, by PID (the app only through its own --state-dir under the QA root);
# never pkill, never qa_wake/qa_app_start (they pkill other scenes' helpers).
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-lim"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
[ -x "$OUT/winid" ] || swiftc -O -o "$OUT/winid" "$REPO/app/qa/winid.swift" 2>/dev/null
PIDS="$QA_ROOT/pids"
SEEDCLI="$REPO/target/release/seedwell"
main_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 ~ /^1[0-9][0-9][0-9]x/ {print $1; exit}'; }
small_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 ~ /^1100x/ {print $1; exit}'; }
shot_main() { qa_cmd "activate main" >/dev/null; sleep 0.4; local id; id=$(main_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }
shot_settings() { qa_cmd "activate settings" >/dev/null; sleep 0.4; local id; id=$(small_id); [ -n "$id" ] || id=$(main_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }

stop_all() {
  for pid in $(pgrep -f "SeedwellApp --state-dir $STATE" 2>/dev/null); do kill "$pid" 2>/dev/null || true; done
  [ -f "$PIDS" ] && for pid in $(cat "$PIDS"); do kill "$pid" 2>/dev/null || true; done
  : > "$PIDS" 2>/dev/null || true
}
trap 'stop_all' EXIT

# One QA scene at a time: wait (up to 30 min) while another one runs.
for i in $(seq 1 180); do
  pgrep -fl 'MacOS/SeedwellApp --state-dir /private/tmp/claude-501' | grep -v "seedwell-qa-lim\|sw-soak" | grep -q SeedwellApp || break
  [ "$i" = 1 ] && echo "another QA scene is running; waiting"
  sleep 10
done
stop_all; sleep 1
rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"; : > "$PIDS"

caffeinate -d -u -t 3000 >/dev/null 2>&1 & echo $! >> "$PIDS"
python3 -I "$REPO/app/qa/qa_tracker.py" --port 7302 --mode ok --interval 1800 --log "$QA_ROOT/tracker.log" & echo $! >> "$PIDS"
sleep 0.6

# Three big torrents on the loopback tracker, seeded by a headless daemon throttled to 2.5 MB/s.
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --count 3 --scale 100 --trackers "z=http://127.0.0.1:7302/announce" >/dev/null
mkdir -p "$QA_ROOT/seeder"
printf 'version = 1\n[transfers]\nupload_limit = 2500000\n' > "$QA_ROOT/seeder/config.toml"
TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/seeder-home" "$SEEDCLI" daemon --state-dir "$QA_ROOT/seeder" --download-dir "$QA_ROOT/seed/data" \
  --listen-port 7301 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$QA_ROOT/seeder/control.sock" >"$QA_ROOT/seeder.log" 2>&1 &
echo $! >> "$PIDS"
for i in $(seq 1 40); do [ -S "$QA_ROOT/seeder/control.sock" ] && break; sleep 0.25; done
for t in "$QA_ROOT/seed/torrents"/*.torrent; do
  "$SEEDCLI" --socket "$QA_ROOT/seeder/control.sock" add "$t" --save-path "$QA_ROOT/seed/data" >/dev/null
done

# The app: a 5 MB/s download limit, a 500 KB/s upload limit, Limit Speed values in both units.
qa_write_config
python3 - "$STATE/config.toml" <<'PY'
import sys, re
p = sys.argv[1]
s = open(p).read()
s = s.replace("[transfers]\n", "[transfers]\ndownload_limit = 5000000\nupload_limit = 500000\n", 1)
s += "\n[transfers.alt]\ndownload_limit = 1500000\nupload_limit = 250000\n"
open(p, "w").write(s)
PY
: > "$CTL"
open -n "$APP" --args --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL" --qa-loopback-trackers --window-size 1180x820 --show-window
for i in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.5; done
sleep 2
for t in "$QA_ROOT/seed/torrents"/*.torrent; do
  "$CLI" --socket "$SOCK" add "$t" --peer 127.0.0.1:7301 >/dev/null || echo "add failed $t"
done
sleep 12

for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "section downloads" >/dev/null; sleep 1
  shot_main "limits-main-$mode"
  qa_cmd "settings transfers" >/dev/null; sleep 1
  shot_settings "limits-settings-$mode"
  qa_cmd "settings close" >/dev/null
  # Before it is a favorite: the hint.
  qa_cmd "tracker-info 127.0.0.1" >/dev/null; sleep 2
  shot_main "limits-tracker-auto-$mode"
  qa_cmd "tracker-info close" >/dev/null
  if [ "$mode" = dark ]; then
    qa_cmd "favorite 127.0.0.1" >/dev/null; sleep 3
    FAV=$("$CLI" --socket "$SOCK" trackers --json 2>/dev/null | python3 -c "import json,sys;d=json.load(sys.stdin)['profiles'];print(d[0]['id'] if d else '')")
    echo "favorite id: $FAV"
    "$CLI" --socket "$SOCK" tracker-limits "$FAV" --down 1MB --up 300k
    sleep 6
  fi
  qa_cmd "tracker-info 127.0.0.1" >/dev/null; sleep 3
  shot_main "limits-tracker-info-$mode"
  qa_cmd "tracker-info close" >/dev/null
done
echo "--- tracker caps in trackers.toml"; cat "$STATE/trackers.toml"
"$CLI" --socket "$SOCK" trackers 2>/dev/null | head -4
echo "done"
