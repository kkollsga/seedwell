#!/usr/bin/env bash
# The quiet "Offline" state (SPEC 7C.4c): the OS path monitor says the network is gone (QA command
# `net offline`, no debounce); the sidebar's rate line and the menu bar menu show "Offline" and
# nothing else changes; `net online` brings the rates back. Own QA root (seedwell-qa-offline), local
# loopback trackers only. Shots: app/build/qa/offline-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-offline"
source "$(dirname "$0")/env.sh"
qa_fresh
qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/swarm" --count 6 >/dev/null
qa_trackers_start
qa_app_start --window-size 1180x820 || qa_end offline
qa_add_all "$QA_ROOT/swarm/torrents" "$QA_ROOT/swarm/data"
qa_until 20 eval '[ "$(qa_count finished)" -ge 6 ]' || { echo "  FAIL: the 6 torrents are not sharing"; FAIL=1; }
for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "section sharing"
  qa_shot "offline-before-$mode"
  qa_cmd "net offline"
  check "the menu says Offline" "$(qa_ask "menu open" "wait menu-titles ~ Offline timeout 5")" "Offline"
  qa_shot "offline-menu-$mode" menu; qa_cmd "menu close"
  qa_shot "offline-window-$mode"
  qa_cmd "net online"
  lacks "the menu's rates are back" "$(qa_ask "menu open" "wait menu-titles !~ Offline timeout 10")" "Offline"
  qa_cmd "menu close"
  qa_shot "offline-after-$mode"
done
qa_end offline
