#!/usr/bin/env bash
# Developer mode (SPEC 8.7): a private-style local tracker (the passkey is in the announce URL), the
# real app with developer mode on, Settings > Advanced and the tracker log viewer with entries, dark
# and light, into app/build/qa/devlog-*.png. Then the proof: the real passkey string is looked for in
# the log files and the script fails when it is there. Own QA root (seedwell-qa-dev).
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-dev"
source "$(dirname "$0")/env.sh"
PASSKEY="0f3a9c52e7b14d68a1c0d9e5f2b7a346"       # the secret the log must never contain
LOGS="$STATE/logs"
qa_fresh

# --- a private-style tracker world: ok, unregistered (failure reason), down (503) -----------------------------
mkdir -p "$QA_ROOT/trk"
for spec in "7301 ok" "7302 unregistered" "7303 down"; do
  set -- $spec
  detach python3 -I "$REPO/app/qa/qa_tracker.py" --port "$1" --mode "$2" --interval 1800 --log "$QA_ROOT/trk/tracker-$1.log" >"$QA_ROOT/trk/$1.out" 2>&1 &
done
qa_until 5 qa_listening 7301 7302 7303
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/swarm" --count 6 --seed 11 \
  --trackers "ok=http://127.0.0.1:7301/announce?passkey=$PASSKEY,unreg=http://127.0.0.1:7302/$PASSKEY/announce,down=http://127.0.0.1:7303/announce?authkey=$PASSKEY&torrent_pass=$PASSKEY" >/dev/null

# --- the app: developer mode on in config.toml ------------------------------------------------------------------
qa_write_config
python3 - "$STATE/config.toml" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace("[advanced]\nmove_limit_mib = 1\n", "[advanced]\nmove_limit_mib = 1\ndeveloper_mode = true\n")
open(p, "w").write(s)
PY
qa_app_start --window-size 1180x820 --show-window || qa_end devlog
qa_add_all "$QA_ROOT/swarm/torrents" "$QA_ROOT/swarm/data"
# The announces (the gate lets one request per second through to each host): every torrent has an entry, the
# refusals and the 503s included.
echo "waiting for the announces ..."
qa_until 40 eval '[ "$(cat "$LOGS"/trackers.log* 2>/dev/null | wc -l)" -ge 6 ] && grep -q unregistered "$QA_ROOT"/trk/tracker-7302.log' || echo "  (few announces so far)"

for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "settings advanced"
  qa_shot "devlog-settings-$mode" settings
  qa_cmd "devlog open" "devlog expand none"
  qa_shot "devlog-viewer-$mode"
  qa_cmd "devlog expand last"
  qa_shot "devlog-viewer-expanded-$mode"
  qa_cmd "devlog errors on" "devlog expand first"
  qa_shot "devlog-viewer-errors-$mode"
  qa_cmd "devlog errors off" "devlog close"
done
s=$(qa_ask "devlog state" | tail -1)
echo "viewer state: $s"
lacks "the viewer has entries" "$s" "devlog: 0 entries"

# --- the proof ------------------------------------------------------------------------------------------------------
echo "== $LOGS"
ls -l "$LOGS" 2>/dev/null
LINES=$(cat "$LOGS"/trackers.log* 2>/dev/null | wc -l | tr -d ' ')
echo "log lines: $LINES"
[ "$LINES" -ge 6 ] || { echo "  FAIL: the log is nearly empty"; FAIL=1; }
if grep -rq "$PASSKEY" "$LOGS"; then echo "  FAIL: the real passkey is in the log:"; grep -rn "$PASSKEY" "$LOGS" | head -3; FAIL=1
else echo "  ok: grep for the real passkey ($PASSKEY) in the log files finds nothing"; fi
echo "    but the trackers did receive it (so the test is meaningful): $(cat "$QA_ROOT"/trk/tracker-730*.log | grep -c "$PASSKEY") request line(s) in their own logs"
grep -rh '"kind":"announce"' "$LOGS" | head -2 | cut -c1-400
KEYS=$(grep -rhoE '"key":"[^"]*"' "$LOGS" | sort | uniq -c)
echo "key field values: $KEYS"
qa_end devlog
