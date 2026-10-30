#!/usr/bin/env bash
# Live numbers for the rows ("45 KB/s (4 of 250)"): a slow second seeder feeds the app's stalled downloads
# and three fresh leechers pull the big torrent from the app again. Run after scenario.sh, any time later.
# Kills only processes whose command line holds this QA root.
set -euo pipefail
source "$(dirname "$0")/env.sh"
qa_guard
for p in $(pgrep -f "seedwell daemon --state-dir $QA_ROOT/(seeder3|leech[123])" || true); do kill "$p" 2>/dev/null || true; done
qa_until 10 eval '! pgrep -f "seedwell daemon --state-dir $QA_ROOT/(seeder3|leech[123])"' || true
# 1. Slow seeder for the held set (the app's downloads that have no source).
rm -rf "$QA_ROOT/seeder3"
qa_seeder 7203 "$QA_ROOT/held/data" 120000 "$QA_ROOT/seeder3"
for t in "$QA_ROOT/held/torrents"/*.torrent; do
  "$TRS" --socket "$SEEDER/control.sock" add "$t" --save-path "$QA_ROOT/held/data" >/dev/null || true
  "$CLI" --socket "$SOCK" add "$t" --peer 127.0.0.1:7203 >/dev/null 2>&1 || true
done
# 2. Fresh leechers on the app's big torrent.
APPPORT=$("$CLI" --socket "$SOCK" status --json | python3 -c "import json,sys;print(json.load(sys.stdin)['totals']['listen_port'])")
for n in 1 2 3; do
  L="$QA_ROOT/leech$n"; rm -rf "$L"; mkdir -p "$L"
  TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/leech-home" detach "$TRS" daemon --state-dir "$L" --download-dir "$L/dl" --listen-port $((7400+n)) --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$L/control.sock" >"$L/log" 2>&1 &
done
for n in 1 2 3; do
  qa_until 10 test -S "$QA_ROOT/leech$n/control.sock"
  "$TRS" --socket "$QA_ROOT/leech$n/control.sock" add "$QA_ROOT/big/torrents"/*.torrent --peer 127.0.0.1:$APPPORT >/dev/null || echo "leecher $n failed"
done
touch "$QA_ROOT/live-ready"
