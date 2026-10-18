#!/usr/bin/env bash
# The file lists under the rows of Downloads and Sharing (SPEC 7A.6 "Later changes"): collapsed list
# (alignment), a season pack expanded with a mixed selection and Play on every episode, priority
# menus, a finished torrent in Sharing, a 2,000-file archive expanded and scrolled, the filter and
# a scroll benchmark; dark and light into app/build/qa/files-*.png. Own QA root (seedwell-qa-files).
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-files"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
TRS="$REPO/target/release/seedwell"
SEEDER_SOCK="$QA_ROOT/seeder/control.sock"

shot_main() { qa_cmd "activate main" >/dev/null; local id; id=$("$OUT/winid" Seedwell | awk '$2==0 {print $1" "$4}' | sort -t' ' -k2 | tail -1 | cut -d' ' -f1); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/files-$1.png" && echo "  files-$1"; }
shot_popups() { local id; for i in 1 2 3 4 5 6 7 8; do id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | head -1); [ -n "$id" ] && break; sleep 0.4; done
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/files-$1.png" && echo "  files-$1" || echo "  MISSING files-$1"; }
ids_of() { "$CLI" --socket "$SOCK" list --json --text "$1" | python3 -c "import json,sys;print(' '.join(t['id'] for t in json.load(sys.stdin)['transfers']))"; }

if [ "${1:-}" != "reuse" ]; then
  bash "$(dirname "$0")/teardown.sh" >/dev/null 2>&1
  sleep 1
  rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"
  qa_write_config
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special season --start 1 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special archive --start 2 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special bunny --start 3 >/dev/null
  mkdir -p "$QA_ROOT/seeder"
  printf 'version = 1\n[transfers]\nupload_limit = 90000\n' > "$QA_ROOT/seeder/config.toml"
  TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/seeder-home" detach "$TRS" daemon --state-dir "$QA_ROOT/seeder" --download-dir "$QA_ROOT/seed/data" --listen-port 7201 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$SEEDER_SOCK" >"$QA_ROOT/seeder.log" 2>&1 &
  for i in $(seq 1 40); do [ -S "$SEEDER_SOCK" ] && break; sleep 0.25; done
  for t in "$QA_ROOT/seed/torrents"/*.torrent; do "$TRS" --socket "$SEEDER_SOCK" add "$t" --save-path "$QA_ROOT/seed/data" >/dev/null; done
  qa_app_start --window-size 1180x820 --show-window
  # the finished ones (data in place) and the downloading pack
  for t in "$QA_ROOT/lib/torrents"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" --save-path "$QA_ROOT/lib/data" >/dev/null 2>&1 || echo "add failed $t"; done
  "$CLI" --socket "$SOCK" add "$QA_ROOT/seed/torrents"/*.torrent --peer 127.0.0.1:7201 >/dev/null
  sleep 3
  for id in $(ids_of Night.Harbor); do
    # take the pack's choice (skip the sample and the nfo), then mix priorities
    "$CLI" --socket "$SOCK" choose "$id" --default >/dev/null 2>&1
  done
  sleep 2
  for id in $(ids_of Night.Harbor); do
    "$CLI" --socket "$SOCK" files "$id" --skip 8,9 --high 0,1 --low 6 >/dev/null 2>&1
  done
  sleep 25
fi

for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "tracker none" "search " "info none" "files collapse-all" "section downloads"
  sleep 1; shot_main "downloads-collapsed-$mode"
  qa_cmd "files expand Night.Harbor"; sleep 2; shot_main "season-expanded-$mode"
  qa_cmd "files select Night.Harbor S01E04"; sleep 0.5; shot_main "season-selected-$mode"
  qa_cmd "files menu Night.Harbor S01E02"; sleep 1; shot_popups "season-filemenu-$mode"; qa_cmd "contextmenu close"
  qa_cmd "files menu Night.Harbor S01E02 priority"; sleep 1; shot_popups "season-prioritymenu-$mode"; qa_cmd "contextmenu close"
  qa_cmd "files collapse Night.Harbor"
  qa_cmd "section sharing"; sleep 1; shot_main "sharing-collapsed-$mode"
  qa_cmd "files expand Bunny"; sleep 2; shot_main "sharing-finished-$mode"
  qa_cmd "files collapse Bunny" "files expand Photo.Archive"; sleep 3; shot_main "archive-$mode"
  qa_cmd "files folders Photo.Archive"; sleep 3; shot_main "archive-folders-$mode"
  qa_cmd "scroll 0.4"; sleep 1; shot_main "archive-scrolled-$mode"
  qa_cmd "files filter Photo.Archive page-0505"; sleep 1.5; shot_main "archive-filter-$mode"
  qa_cmd "files filter Photo.Archive reel"; sleep 1.5; shot_main "archive-filter-video-$mode"
  qa_cmd "files filter Photo.Archive " "scroll top" "files collapse-all"
done
if [ "${BENCH:-1}" = "1" ]; then
  qa_cmd "section sharing" "files expand Photo.Archive"; sleep 3; qa_cmd "files folders Photo.Archive" "scroll top"; sleep 3
  rm -f "$CTL.bench"; qa_cmd "bench-scroll"; sleep 25; echo "scroll bench (2,000-file archive expanded):"; cat "$CTL.bench" 2>/dev/null | tail -4
fi
