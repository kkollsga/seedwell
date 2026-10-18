#!/usr/bin/env bash
# Live numbers for the rows ("45 KB/s (4 of 250)"): a slow second seeder feeds the app's stalled downloads
# and three fresh leechers pull the big torrent from the app again. Run after scenario.sh, any time later.
# Kills only processes whose command line holds this QA root.
set -euo pipefail
source "$(dirname "$0")/env.sh"
qa_guard
S="$REPO/target/release/seedwell"
for p in $(pgrep -f "seedwell daemon --state-dir $QA_ROOT/(seeder3|leech[123])" || true); do kill "$p" 2>/dev/null || true; done
sleep 1
# 1. Slow seeder for the held set (the app's downloads that have no source).
D="$QA_ROOT/seeder3"; rm -rf "$D"; mkdir -p "$D"
printf 'version = 1\n[transfers]\nupload_limit = 120000\n' > "$D/config.toml"
TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/seeder-home" detach "$S" daemon --state-dir "$D" --download-dir "$QA_ROOT/held/data" --listen-port 7203 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$D/control.sock" >"$D/log" 2>&1 &
for i in $(seq 1 40); do [ -S "$D/control.sock" ] && break; sleep 0.25; done
for t in "$QA_ROOT/held/torrents"/*.torrent; do
  "$S" --socket "$D/control.sock" add "$t" --save-path "$QA_ROOT/held/data" >/dev/null || true
  "$CLI" --socket "$SOCK" add "$t" --peer 127.0.0.1:7203 >/dev/null 2>&1 || true
done
# 2. Fresh leechers on the app's big torrent.
APPPORT=$("$CLI" --socket "$SOCK" status --json | python3 -c "import json,sys;print(json.load(sys.stdin)['totals']['listen_port'])")
for n in 1 2 3; do
  L="$QA_ROOT/leech$n"; rm -rf "$L"; mkdir -p "$L"
  TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/leech-home" detach "$S" daemon --state-dir "$L" --download-dir "$L/dl" --listen-port $((7400+n)) --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$L/control.sock" >"$L/log" 2>&1 &
done
sleep 3
for n in 1 2 3; do
  "$S" --socket "$QA_ROOT/leech$n/control.sock" add "$QA_ROOT/big/torrents"/*.torrent --peer 127.0.0.1:$APPPORT >/dev/null || echo "leecher $n failed"
done
