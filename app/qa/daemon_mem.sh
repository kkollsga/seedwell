#!/usr/bin/env bash
# Engine-only baseline: a headless `seedwell daemon` with the same N seeding torrents; prints its
# footprint over time (is growth the engine's or the app's?).
set -euo pipefail
source "$(dirname "$0")/env.sh"
qa_guard
D="$QA_ROOT/daemon"
pkill -f "seedwell daemon --state-dir $D" 2>/dev/null || true
rm -rf "$D"; mkdir -p "$D"
qa_port_free 7301 || exit 1
TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/daemon-home" detach "$REPO/target/release/seedwell" daemon --state-dir "$D" --download-dir "$SWARM/data" --listen-port 7301 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$D/control.sock" > "$D/log" 2>&1 &
for i in $(seq 1 40); do [ -S "$D/control.sock" ] && break; sleep 0.25; done
PID=$(pgrep -f "seedwell daemon --state-dir $D" | head -1)
for t in "$SWARM/torrents"/*.torrent; do "$REPO/target/release/seedwell" --socket "$D/control.sock" add "$t" --save-path "$SWARM/data" >/dev/null 2>&1 || true; done
for s in 30 90 180; do
  sleep $((s == 30 ? 30 : s == 90 ? 60 : 90))
  echo "t+${s}s: $(footprint -p "$PID" | grep 'phys_footprint:' | head -1)  $(top -l 1 -pid "$PID" -stats cpu,idlew | tail -1)"
done
pkill -f "seedwell daemon --state-dir $D"
