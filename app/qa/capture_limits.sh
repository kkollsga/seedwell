#!/usr/bin/env bash
# Speed limits (SPEC 7, 7E.4, 8.3; own QA root seedwell-qa-lim): Settings > Transfers with the number +
# unit controls (MB/s and KB/s values, empty = Unlimited), a favorite tracker's Info page with shared
# caps and its current rate against them, the same tracker before it is a favorite ("Save as Favorite to
# set limits"), and the main window's speed line with the limit marker ("↓ 2.4 / 5 MB/s"). A throttled
# headless seeder feeds three downloads. Dark and light, into app/build/qa/limits-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-lim"
source "$(dirname "$0")/env.sh"
qa_fresh
mkdir -p "$QA_ROOT/trk"
detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7302 --mode ok --interval 1800 --log "$QA_ROOT/trk/7302.log" >"$QA_ROOT/trk/7302.out" 2>&1 &
qa_until 5 qa_listening 7302

# Three big torrents on the loopback tracker, seeded by a headless daemon throttled to 2.5 MB/s.
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --count 3 --scale 100 --trackers "z=http://127.0.0.1:7302/announce" >/dev/null
qa_seeder 7301 "$QA_ROOT/seed/data" 2500000 || qa_end limits
qa_seed "$QA_ROOT/seed/data" "$QA_ROOT/seed/torrents"/*.torrent

# The app: a 5 MB/s download limit, a 500 KB/s upload limit, Limit Speed values in both units.
qa_write_config
python3 - "$STATE/config.toml" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace("[transfers]\n", "[transfers]\ndownload_limit = 5000000\nupload_limit = 500000\n", 1)
s += "\n[transfers.alt]\ndownload_limit = 1500000\nupload_limit = 250000\n"
open(p, "w").write(s)
PY
qa_app_start --window-size 1180x820 --show-window || qa_end limits
for t in "$QA_ROOT/seed/torrents"/*.torrent; do
  "$CLI" --socket "$SOCK" add "$t" --peer 127.0.0.1:7301 >/dev/null || echo "add failed $t"
done
# Data flows (the speed line shows a rate against the limit).
down_rate() { "$CLI" --socket "$SOCK" status --json 2>/dev/null | python3 -c "import json,sys;print(json.load(sys.stdin)['totals']['down_rate'])"; }
qa_until 30 eval '[ "$(down_rate)" -gt 100000 ]' || echo "  (no download rate yet)"

for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "section downloads"
  qa_shot "limits-main-$mode"
  qa_cmd "settings transfers"
  qa_shot "limits-settings-$mode" settings
  qa_cmd "settings close"
  # Before it is a favorite: the hint.
  qa_cmd "tracker-info 127.0.0.1"
  qa_shot "limits-tracker-auto-$mode"
  qa_cmd "tracker-info close"
  if [ "$mode" = dark ]; then
    qa_cmd "favorite 127.0.0.1"
    fav() { "$CLI" --socket "$SOCK" trackers --json 2>/dev/null | python3 -c "import json,sys;d=json.load(sys.stdin)['profiles'];print(d[0]['id'] if d else '')"; }
    qa_until 10 eval '[ -n "$(fav)" ]' || { echo "  FAIL: no favorite"; FAIL=1; }
    FAV=$(fav); echo "favorite id: $FAV"
    "$CLI" --socket "$SOCK" tracker-limits "$FAV" --down 1MB --up 300k
  fi
  qa_cmd "tracker-info 127.0.0.1"
  qa_shot "limits-tracker-info-$mode"
  qa_cmd "tracker-info close"
done
echo "--- tracker caps in trackers.toml"; cat "$STATE/trackers.toml"
"$CLI" --socket "$SOCK" trackers 2>/dev/null | head -4
qa_end limits
