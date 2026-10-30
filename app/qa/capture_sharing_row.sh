#!/usr/bin/env bash
# Sharing row lines and size capsules (SPEC 8.3): a season pack downloading from a throttled seeder (rates
# on the files), then finished in Sharing with part-way skipped files (partly filled capsules). Own QA root
# seedwell-qa-sharingrow; shots files-sharingrow-*.png in app/build/qa.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-sharingrow"
source "$(dirname "$0")/env.sh"
ids_of() { "$CLI" --socket "$SOCK" list --json --text "$1" | python3 -c "import json,sys;print(' '.join(t['id'] for t in json.load(sys.stdin)['transfers']))"; }
finished() { "$CLI" --socket "$SOCK" list --json --text Night.Harbor | grep -qE '"state": "(seeding|low-power)"'; }

qa_fresh
qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special season --start 1 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special archive --start 2 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special bunny --start 3 >/dev/null
qa_seeder 7201 "$QA_ROOT/seed/data" 90000 || qa_end sharing_row
qa_seed "$QA_ROOT/seed/data" "$QA_ROOT/seed/torrents"/*.torrent
qa_app_start --window-size 1180x820 --show-window || qa_end sharing_row
# the finished ones (data in place) and the downloading pack
for t in "$QA_ROOT/lib/torrents"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" --save-path "$QA_ROOT/lib/data" >/dev/null 2>&1 || echo "add failed $t"; done
"$CLI" --socket "$SOCK" add "$QA_ROOT/seed/torrents"/*.torrent --peer 127.0.0.1:7201 >/dev/null
qa_until 10 eval '[ -n "$(ids_of Night.Harbor)" ]'
for id in $(ids_of Night.Harbor); do "$CLI" --socket "$SOCK" start "$id" --suggested >/dev/null 2>&1; done
# the throttled transfer runs a while, so files 5 to 9 are part way when they are skipped (they stay partial)
sleep 14
for id in $(ids_of Night.Harbor); do "$CLI" --socket "$SOCK" files "$id" --skip 5,6,7,8,9 >/dev/null 2>&1; done
qa_cmd "appearance dark" "window open" "window size 1180x820" "tracker none" "search " "info none" "files collapse-all" "section downloads"
qa_cmd "files expand Night.Harbor"; qa_wait "ui state !~ :-" 10 >/dev/null; qa_shot "files-sharingrow-downloading-dark"
qa_until 180 finished || { echo "  FAIL: Night.Harbor did not finish in 3 minutes (the Sharing shots will not show it)"; FAIL=1; }
for mode in dark light; do
  qa_cmd "appearance $mode" "section sharing" "files expand Night.Harbor"; qa_wait "ui state !~ :-" 10 >/dev/null; qa_shot "files-sharingrow-$mode"
done
qa_end sharing_row
