#!/usr/bin/env bash
# Builds the standard QA scene: an isolated app with real data (sharing in all groups, active
# downloads from a local seeder, queued items, a held torrent) and leaves it running.
#   scenario.sh [N seeding torrents, default 60]
set -euo pipefail
source "$(dirname "$0")/env.sh"
N="${1:-60}"
SWARM="$QA_ROOT/scene"   # the scene has its own swarm; perf.sh uses $QA_ROOT/swarm
shift || true
qa_guard
qa_app_stop 2>/dev/null || true; qa_trackers_stop 2>/dev/null || true
pkill -f "seedwell daemon --state-dir $QA_ROOT" 2>/dev/null || true
rm -rf "$QA_ROOT/leech1" "$QA_ROOT/leech2" "$QA_ROOT/leech3" "$QA_ROOT/big" "$QA_ROOT/held" "$QA_ROOT/scene"
sleep 1
rm -rf "$STATE" "$HOMEROOT" "$QA_ROOT/seed2" "$QA_ROOT/seeder" "$QA_ROOT/trk"
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

[[tracker]]
id = "tracker-x"
name = "tracker-x.org"
urls = ["http://127.0.0.1:7103/announce"]
TOML
cat > "$STATE/rules.toml" <<'TOML'
version = 1

[default]
goal = { kind = "ratio_or_time", ratio = 1.0, time = "7d" }

[[rule]]
id = "y"
name = "Y – seed to 1.0"
match = { tracker = ["tracker-y"] }
goal = { kind = "ratio", ratio = 1.0 }

[[rule]]
id = "z"
name = "Z – priority, H&R safe"
match = { tracker = ["tracker-z"] }
priority = "high"
goal = { kind = "ratio", ratio = 2.0 }
min_seed_time = "72h"

[[rule]]
id = "public-software"
name = "Public software"
match = { tracker = ["public"], category = ["software"] }
goal = { kind = "ratio", ratio = 3.0 }
TOML
if [ ! -f "$SWARM/manifest.json" ] || [ "$(python3 -c "import json;print(len(json.load(open('$SWARM/manifest.json'))))")" -lt "$N" ]; then
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$SWARM" --count "$N" \
    --trackers "z=http://127.0.0.1:7101/announce,y=http://127.0.0.1:7102/announce,y=http://127.0.0.1:7102/announce,public=,z=http://127.0.0.1:7101/announce,public=,x=http://127.0.0.1:7103/announce,y=http://127.0.0.1:7102/announce,q=http://localhost:7104/announce,public="
fi
# Big, throttled downloads from a local seeder (a headless seedwell daemon).
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed2" --count 6 --start 1000 --scale 60 --trackers "z=http://127.0.0.1:7101/announce,public=" >/dev/null
mkdir -p "$QA_ROOT/seeder"
cat > "$QA_ROOT/seeder/config.toml" <<'TOML'
version = 1
[transfers]
upload_limit = 1500000
TOML
qa_trackers_start
TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/seeder-home" detach "$REPO/target/release/seedwell" daemon --state-dir "$QA_ROOT/seeder" --download-dir "$QA_ROOT/seed2/data" --listen-port 7201 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$QA_ROOT/seeder/control.sock" >"$QA_ROOT/seeder.log" 2>&1 &
for i in $(seq 1 40); do [ -S "$QA_ROOT/seeder/control.sock" ] && break; sleep 0.25; done
for t in "$QA_ROOT/seed2/torrents"/*.torrent; do
  "$REPO/target/release/seedwell" --socket "$QA_ROOT/seeder/control.sock" add "$t" --save-path "$QA_ROOT/seed2/data" >/dev/null
done
qa_app_start --window-size 1180x820 --show-window "$@"
qa_add_all "$SWARM/torrents" "$SWARM/data"
# Most of the seeds rest: rewrite their last activity to three days ago while the app is closed.
sleep 4
qa_cmd "quit"
for i in $(seq 1 120); do pgrep -f "SeedwellApp --state-dir $STATE" >/dev/null || break; sleep 1; done
python3 -I "$REPO/app/qa/age.py" "$STATE" --keep 6
qa_app_start --window-size 1180x820 --show-window "$@"
for t in "$QA_ROOT/seed2/torrents"/*.torrent; do
  "$CLI" --socket "$SOCK" add "$t" --peer 127.0.0.1:7201 >/dev/null || echo "download add failed $t"
done
# A few multi-file torrents that the engine holds for a choice (SPEC 7A.6).
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/held" --count 7 --start 2000 --trackers "y=http://127.0.0.1:7102/announce,z=http://127.0.0.1:7101/announce" >/dev/null
"$CLI" --socket "$SOCK" settings set transfers.ask_before_adding=multi_file >/dev/null
sleep 3
for t in "$QA_ROOT/held/torrents"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" >/dev/null 2>&1 || true; done
"$(dirname "$0")/popular.sh"
# The move-progress row needs another volume; skip quietly when a disk image cannot be made.
"$(dirname "$0")/moving.sh" || echo "moving.sh failed (no Moving row in this scene)"
# A download that does not fit on its (tiny) volume: the "Not enough space" row.
"$(dirname "$0")/nospace.sh" || echo "nospace.sh failed (no Not-enough-space row in this scene)"
"$(dirname "$0")/extras.sh" || echo "extras.sh failed (no Big Buck Bunny / private-tracker torrents in this scene)"
echo "scene ready"
