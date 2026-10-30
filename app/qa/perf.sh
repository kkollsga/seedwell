#!/usr/bin/env bash
# Performance run: N seeding torrents (local mock trackers, real engine), window closed, then
# measured idle memory and CPU, snapshot cost and list scrolling with the window open.
#   perf.sh [N=500]            Uses the app currently built in app/build (use a full-LTO build for numbers).
set -euo pipefail
source "$(dirname "$0")/env.sh"
N="${1:-500}"
qa_guard
qa_app_stop || true; qa_trackers_stop || true
sleep 1
rm -rf "$STATE" "$HOMEROOT" "$QA_ROOT/trk"
mkdir -p "$QA_ROOT"
qa_write_config
cat > "$STATE/trackers.toml" <<'TOML'
version = 1
[[tracker]]
id = "tracker-z"
name = "tracker-z.cc"
urls = ["http://127.0.0.1:7101/announce"]
[[tracker]]
id = "tracker-y"
name = "tracker-y.net"
urls = ["http://127.0.0.1:7102/announce"]
TOML
if [ ! -f "$SWARM/manifest.json" ] || [ "$(python3 -c "import json;print(len(json.load(open('$SWARM/manifest.json'))))")" -lt "$N" ]; then
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$SWARM" --count "$N" \
    --trackers "z=http://127.0.0.1:7101/announce,y=http://127.0.0.1:7102/announce,public=,y=http://127.0.0.1:7102/announce,public=,z=http://127.0.0.1:7101/announce"
fi
qa_trackers_start
t0=$(date +%s)
qa_app_start        # window closed
PID=$(pgrep -f "SeedwellApp --state-dir $STATE" | head -1)
echo "app pid $PID, started"
qa_add_all "$SWARM/torrents" "$SWARM/data"
echo "all added in $(( $(date +%s) - t0 )) s"
for i in $(seq 1 120); do
  n=$("$CLI" --socket "$SOCK" status 2>/dev/null | grep -o "[0-9]* sharing" | head -1 | cut -d' ' -f1 || true)
  [ "${n:-0}" -ge "$N" ] && break; sleep 2
done
echo "sharing: ${n:-0}"
sleep "${PERF_SETTLE:-60}"
echo "== idle, window closed =="
ps -o rss=,vsz=,%cpu= -p "$PID" | awk '{printf "RSS %.1f MB\n", $1/1024}'
footprint -p "$PID" 2>/dev/null | grep -E "phys_footprint|Footprint:" | head -3 || true
top -l 3 -s 10 -pid "$PID" -stats pid,cpu,mem,idlew,power 2>/dev/null | tail -3
echo "== window open =="
qa_cmd "window size 1180x820" "section sharing"
sleep 3   # the window settles before its memory is read
ps -o rss= -p "$PID" | awk '{printf "RSS with window %.1f MB\n", $1/1024}'
: > "$CTL.bench"
qa_cmd "bench-snapshot"
qa_cmd "bench-scroll"
qa_until 30 grep -q '^scroll:' "$CTL.bench" || echo "scroll bench: no frames in 30 s (display asleep?)"
cat "$CTL.bench"
echo "== window closed again =="
qa_cmd "window close"
sleep 5
ps -o rss= -p "$PID" | awk '{printf "RSS after closing %.1f MB\n", $1/1024}'
footprint -p "$PID" 2>/dev/null | grep -E "phys_footprint|Footprint:" | head -3 || true
