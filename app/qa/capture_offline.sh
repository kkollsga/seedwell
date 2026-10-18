#!/usr/bin/env bash
# The quiet "Offline" state (SPEC 7C.4c): the OS path monitor says the network is gone (QA command
# `net offline`, no debounce); the sidebar's rate line and the menu bar menu show "Offline" and
# nothing else changes; `net online` brings the rates back. Own QA root (seedwell-qa-offline), local
# loopback trackers only. Shots: app/build/qa/offline-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-offline"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
[ -x "$OUT/winid" ] || swiftc -O -o "$OUT/winid" "$REPO/app/qa/winid.swift" 2>/dev/null
main_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 ~ /^1[0-9][0-9][0-9]x/ {print $1; exit}'; }
shot_main() { qa_cmd "activate main" >/dev/null; local id; id=$(main_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }
shot_menu() { qa_cmd "menu open" >/dev/null; sleep 1.2; local id; id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | head -1); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; qa_cmd "menu close" >/dev/null; }
snap_line() { "$CLI" --socket "$SOCK" status 2>/dev/null | head -3; }

qa_app_stop; sleep 1
rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"
qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/swarm" --count 6 >/dev/null
qa_trackers_start
qa_app_start --window-size 1180x820
sleep 2
qa_add_all "$QA_ROOT/swarm/torrents" "$QA_ROOT/swarm/data"
sleep 4
for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "section sharing" >/dev/null; sleep 1
  shot_main "offline-before-$mode"
  qa_cmd "net offline" >/dev/null; sleep 2.5
  shot_main "offline-window-$mode"
  shot_menu "offline-menu-$mode"
  qa_cmd "net online" >/dev/null; sleep 2.5
  shot_main "offline-after-$mode"
done
echo "done; tear down with app/qa/teardown.sh (QA_ROOT=$QA_ROOT)"
