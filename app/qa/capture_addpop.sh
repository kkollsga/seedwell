#!/usr/bin/env bash
# File selection inside the notifier popover (SPEC 7A.6, 7A.7, 8.6): a multi-file torrent awaiting a
# choice with the checklist in the popover, an auto-start torrent after "Choose Files…" expanded, two
# torrents in one popover, and the main window reflecting a selection made in the popover. The popover
# is captured by its own window id; never the desktop. Own QA root (seedwell-qa-addpop); shots go to
# app/build/qa/addpop-*.png. Only processes whose command line carries this root's --state-dir are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-addpop"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
REPORT="$CTL.bench"
qa_pid() { pgrep -f "SeedwellApp --state-dir $STATE" | head -1; }
add() { "$CLI" --socket "$SOCK" add "$@" >/dev/null 2>&1 || echo "add failed: $*"; }
set_ask() { sed -i '' "s/^ask_before_adding = .*/ask_before_adding = \"$1\"/" "$STATE/config.toml"; sleep 5; }
state() { rm -f "$REPORT"; qa_cmd "starting state" >/dev/null; tail -1 "$REPORT" 2>/dev/null; }
# The popover is the one window above the menu level that is about 320 wide.
popover_line() { for i in 1 2 3 4 5 6 7 8; do l=$("$OUT/winid" Seedwell | awk '$2>0 && $4 ~ /^3[0-9][0-9]x/ {print; exit}'); [ -n "$l" ] && break; sleep 0.3; done; echo "$l"; }
shot_popover() { # name
  local l id; l=$(popover_line); id=${l%% *}
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/addpop-$1.png" && echo "  addpop-$1 (popover window $id, $(echo "$l" | awk '{print $4}'))" || echo "  MISSING addpop-$1"
}
shot_main() { qa_cmd "activate main" >/dev/null; local id; id=$("$OUT/winid" Seedwell | awk '$2==0 {print $1" "$4}' | sort -t' ' -k2 | tail -1 | cut -d' ' -f1); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/addpop-$1.png" && echo "  addpop-$1"; }

if [ "${1:-}" != "reuse" ]; then
  pkill -f "SeedwellApp --state-dir $STATE" 2>/dev/null; sleep 1
  rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"
  QA_START_DELAY=120 qa_write_config
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special season --start 1 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special bunny --start 3 >/dev/null
  qa_app_start --window-size 1180x820 --show-window
  sleep 2
fi
SEASON=$(ls "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent | head -1)
BUNNY=$(ls "$QA_ROOT"/lib/torrents/*Bunny*.torrent | head -1)
qa_cmd "appearance dark" >/dev/null

echo "== auto-start torrent, Choose Files… expanded in the popover"
add "$BUNNY"; sleep 2; echo "$(state)"
qa_cmd "starting show" >/dev/null; sleep 1; shot_popover "countdown"
qa_cmd "starting choose" >/dev/null; sleep 2; echo "$(state)"; shot_popover "choose-expanded"
qa_cmd "starting cancel" >/dev/null; sleep 2; echo "after Cancel: $(state)"; "$CLI" --socket "$SOCK" list 2>&1 | head -5

echo "== multi-file torrent awaiting a choice: compact first, the chevron opens it in place"
set_ask multi_file
add "$SEASON"; sleep 3; echo "$(state)"
shot_popover "compact"
qa_cmd "starting fold" >/dev/null; sleep 1; echo "$(state)"      # the chevron in front of the title
shot_popover "select-expanded-closed"
qa_cmd "starting open" >/dev/null; sleep 1; shot_popover "select-expanded"

echo "== two torrents in one popover: one expanded, the other compact"
set_ask never
add "$BUNNY"; sleep 3; echo "$(state)"
shot_popover "two-compact"
qa_cmd "starting choose Bunny" >/dev/null; sleep 1; echo "$(state)"   # the chevron on the other: this one closes
shot_popover "two-switched"
qa_cmd "starting cancel Bunny" >/dev/null; sleep 1
qa_cmd "starting fold Night" >/dev/null; sleep 1; echo "$(state)"

echo "== a selection made in the popover shows in the main window"
qa_cmd "starting none" "starting toggle S01E02" "starting toggle S01E03" >/dev/null; sleep 1
echo "$(state)"
shot_popover "selection"
qa_cmd "window open" "section downloads" "chooser first" >/dev/null; sleep 2
shot_main "chooser-main"
qa_cmd "chooser-close" >/dev/null
qa_cmd "starting ok Night.Harbor" >/dev/null; sleep 3; echo "after OK: $(state)"
shot_main "row-after-ok"
