#!/usr/bin/env bash
# QA for the follow-ups (Transmission's port on the first launch):
#   - Settings > Network: "Transmission uses port 51413  Use it" on the first launch, read from a
#     FIXTURE plist (--qa-transmission-prefs); the real Transmission preferences are never read.
# Own QA root; everything isolated under it. Shots go to app/build/qa/followup-*.png, dark and light.
# Tear down afterwards with app/qa/teardown.sh (it needs the same QA_ROOT).
set -uo pipefail
export QA_ROOT="${QA_ROOT:-/private/tmp/claude-501/seedwell-qa-fu}"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
qa_app_stop 2>/dev/null || true
rm -rf "$STATE" "$HOMEROOT" "$QA_ROOT/fu"
mkdir -p "$QA_ROOT/fu"

# A fixture standing in for Transmission's preferences (binary plist, like the real one).
python3 -I - "$QA_ROOT/fu/org.m0k.transmission.plist" <<'PY'
import plistlib, sys
with open(sys.argv[1], "wb") as f:
    plistlib.dump({"BindPort": 51413, "DownloadFolder": "/nowhere"}, f, fmt=plistlib.FMT_BINARY)
PY

qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/fu/swarm" --count 3 --start 0 >/dev/null
# A local seeder (headless daemon) has the data; the app downloads it, finishes it into the download
# folder.
mkdir -p "$QA_ROOT/fu/seeder"
printf 'version = 1\n' > "$QA_ROOT/fu/seeder/config.toml"
TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/fu/seeder-home" detach "$REPO/target/release/seedwell" daemon --state-dir "$QA_ROOT/fu/seeder" --download-dir "$QA_ROOT/fu/swarm/data" --listen-port 7211 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$QA_ROOT/fu/seeder/control.sock" >"$QA_ROOT/fu/seeder.log" 2>&1 &
for i in $(seq 1 40); do [ -S "$QA_ROOT/fu/seeder/control.sock" ] && break; sleep 0.25; done
for t in "$QA_ROOT/fu/swarm/torrents"/*.torrent; do
  "$REPO/target/release/seedwell" --socket "$QA_ROOT/fu/seeder/control.sock" add "$t" --save-path "$QA_ROOT/fu/swarm/data" >/dev/null
done
qa_app_start --window-size 1180x820 --show-window --qa-transmission-prefs "$QA_ROOT/fu/org.m0k.transmission.plist"
for t in "$QA_ROOT/fu/swarm/torrents"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" --peer 127.0.0.1:7211 >/dev/null 2>&1 || echo "add failed: $t"; done
sleep 10

main_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 ~ /^1[0-9][0-9][0-9]x/ {print $1; exit}'; }
small_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 !~ /^1[1-9][0-9][0-9]x/ {print $1; exit}'; }
shot_main() { qa_cmd "activate main" >/dev/null; local id; id=$(main_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }
shot_other() { qa_cmd "activate $2" >/dev/null; local id; id=$(small_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }

"$CLI" --socket "$SOCK" list --json 2>/dev/null | python3 -c "
import json,sys
for t in json.load(sys.stdin)['transfers']: print(' ', t['name'], t.get('category'), t.get('note'))"
for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "section sharing" "scroll top"
  shot_main "followup-sharing-$mode"
  qa_cmd "settings network" "settings-scroll top"; sleep 0.6; shot_other "followup-transmission-port-$mode" settings
  qa_cmd "settings close"
done
qa_cmd "appearance dark"
