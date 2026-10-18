#!/usr/bin/env bash
# Popular needs three peers downloading from the app: three headless leechers fetch a big torrent from it.
# Run after the app's final start (the listen port is random per launch).
set -euo pipefail
source "$(dirname "$0")/env.sh"
S="$REPO/target/release/seedwell"
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/big" --count 1 --start 4 --scale 400 --trackers "public=" >/dev/null
# The scene's throttled downloads fill every download slot; a torrent that is still being verified
# would wait in that queue and never be shared. Open the slots while it is added, then close them.
"$CLI" --socket "$SOCK" settings set transfers.max_active_downloads=10 >/dev/null
"$CLI" --socket "$SOCK" add "$QA_ROOT/big/torrents"/*.torrent --save-path "$QA_ROOT/big/data" >/dev/null || true
BIGID=$("$CLI" --socket "$SOCK" list --json --limit 500 | python3 -c "
import json,sys
print(next(t['id'] for t in json.load(sys.stdin)['transfers'] if '/big/data/' in t['save_path']))")
for i in $(seq 1 90); do
  st=$("$CLI" --socket "$SOCK" info "$BIGID" --no-files --no-peers --no-trackers --json 2>/dev/null | python3 -c "import json,sys;print(json.load(sys.stdin)['transfer']['state'])" 2>/dev/null || true)
  case "$st" in seeding|low-power) break;; esac; sleep 1
done
"$CLI" --socket "$SOCK" settings set transfers.max_active_downloads=3 >/dev/null
"$CLI" --socket "$SOCK" limits --up 700KB >/dev/null
APPPORT=$("$CLI" --socket "$SOCK" status --json | python3 -c "import json,sys;print(json.load(sys.stdin)['totals']['listen_port'])")
for n in 1 2 3; do
  L="$QA_ROOT/leech$n"; mkdir -p "$L"
  TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/leech-home" detach "$S" daemon --state-dir "$L" --download-dir "$L/dl" --listen-port $((7400+n)) --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$L/control.sock" >"$L/log" 2>&1 &
done
sleep 3
for n in 1 2 3; do
  "$S" --socket "$QA_ROOT/leech$n/control.sock" add "$QA_ROOT/big/torrents"/*.torrent --peer 127.0.0.1:$APPPORT >/dev/null || echo "leecher $n failed"
done
