#!/usr/bin/env bash
# The file lists under the rows of Downloads and Sharing (SPEC 7A.6 "Later changes"): collapsed list
# (alignment), a season pack expanded with a mixed selection and Play on every episode, priority
# menus, a finished torrent in Sharing, a 2,000-file archive expanded and scrolled, the filter and
# a scroll benchmark; dark and light into app/build/qa/files-*.png. Own QA root (seedwell-qa-files).
#   capture_files.sh [scene]     scene: only build the scene and leave it running (capture_shell.sh uses it)
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-files"
source "$(dirname "$0")/env.sh"
# An expanded file list is loaded (`ui state` lists each open tree as id:wanted/total, "-" until it is read).
tree_loaded() { qa_wait "ui state !~ :-" 10 >/dev/null || { echo "  FAIL: a file list did not load"; FAIL=1; }; }
ids_of() { "$CLI" --socket "$SOCK" list --json --text "$1" | python3 -c "import json,sys;print(' '.join(t['id'] for t in json.load(sys.stdin)['transfers']))"; }

qa_fresh
qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special season --start 1 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special archive --start 2 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special bunny --start 3 >/dev/null
qa_seeder 7201 "$QA_ROOT/seed/data" 90000 || qa_end files
qa_seed "$QA_ROOT/seed/data" "$QA_ROOT/seed/torrents"/*.torrent
qa_app_start --window-size 1180x820 --show-window || qa_end files
# the finished ones (data in place) and the downloading pack
for t in "$QA_ROOT/lib/torrents"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" --save-path "$QA_ROOT/lib/data" >/dev/null 2>&1 || echo "add failed $t"; done
"$CLI" --socket "$SOCK" add "$QA_ROOT/seed/torrents"/*.torrent --peer 127.0.0.1:7201 >/dev/null
qa_until 10 eval '[ -n "$(ids_of Night.Harbor)" ]'
# take the pack's choice (skip the sample and the nfo), then mix priorities
for id in $(ids_of Night.Harbor); do "$CLI" --socket "$SOCK" start "$id" --suggested >/dev/null 2>&1; done
for id in $(ids_of Night.Harbor); do qa_until 10 "$CLI" --socket "$SOCK" files "$id" --skip 8,9 --high 0,1 --low 6; done
# the finished ones are checked and sharing, the pack has data coming in
qa_until 30 eval '[ "$(qa_count finished)" -ge 2 ]' || echo "  (the finished torrents are not sharing yet)"
qa_until 30 eval '"$CLI" --socket "$SOCK" list downloading 2>/dev/null | grep -q "Night"' || echo "  (the pack is not downloading)"
[ "${1:-}" = scene ] && { echo "scene ready ($QA_ROOT)"; exit 0; }

for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "tracker none" "search " "info none" "files collapse-all" "section downloads"
  qa_shot "files-downloads-collapsed-$mode"
  qa_cmd "files expand Night.Harbor"; tree_loaded; qa_shot "files-season-expanded-$mode"
  qa_cmd "files select Night.Harbor S01E04"; qa_shot "files-season-selected-$mode"
  qa_cmd "files menu Night.Harbor S01E02"; qa_shot "files-season-filemenu-$mode" menu; qa_cmd "contextmenu close"
  qa_cmd "files menu Night.Harbor S01E02 priority"; qa_shot "files-season-prioritymenu-$mode" menu; qa_cmd "contextmenu close"
  qa_cmd "files collapse Night.Harbor"
  qa_cmd "section sharing"; qa_shot "files-sharing-collapsed-$mode"
  qa_cmd "files expand Bunny"; tree_loaded; qa_shot "files-sharing-finished-$mode"
  qa_cmd "files collapse Bunny" "files expand Photo.Archive"; tree_loaded; qa_shot "files-archive-$mode"
  qa_cmd "files folders Photo.Archive"; qa_shot "files-archive-folders-$mode"
  qa_cmd "scroll 0.4"; qa_shot "files-archive-scrolled-$mode"
  qa_cmd "files filter Photo.Archive page-0505"; qa_shot "files-archive-filter-$mode"
  qa_cmd "files filter Photo.Archive reel"; qa_shot "files-archive-filter-video-$mode"
  qa_cmd "files filter Photo.Archive " "scroll top" "files collapse-all"
done
if [ "${BENCH:-1}" = "1" ]; then
  qa_cmd "section sharing" "files expand Photo.Archive"; tree_loaded; qa_cmd "files folders Photo.Archive" "scroll top"
  # Frames come from the display link, which needs an awake display: without one the bench reports nothing.
  rm -f "$CTL.bench"; qa_cmd "bench-scroll"
  if qa_until 30 grep -q '^scroll:' "$CTL.bench"; then echo "scroll bench (2,000-file archive expanded):"; grep '^scroll:' "$CTL.bench"
  else echo "scroll bench: no frames in 30 s (display asleep?)"; fi
fi
qa_end files
