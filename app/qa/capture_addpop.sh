#!/usr/bin/env bash
# File selection inside the notifier popover (SPEC 7A.6, 7A.7, 8.6): a multi-file torrent awaiting a
# choice with the checklist in the popover, an auto-start torrent after "Choose Files…" expanded, two
# torrents in one popover, and the main window reflecting a selection made in the popover. Own QA root
# (seedwell-qa-addpop); shots go to app/build/qa/addpop-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-addpop"
source "$(dirname "$0")/env.sh"
add() { "$CLI" --socket "$SOCK" add "$@" >/dev/null 2>&1 || echo "add failed: $*"; }
state() { qa_ask "starting state" | tail -1; }
shot_popover() { qa_shot "addpop-$1" popover; }
shot_main() { qa_shot "addpop-$1"; }

qa_fresh
QA_START_DELAY=120 qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special season --start 1 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special bunny --start 3 >/dev/null
qa_app_start --window-size 1180x820 --show-window || qa_end addpop
SEASON=$(ls "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent | head -1)
BUNNY=$(ls "$QA_ROOT"/lib/torrents/*Bunny*.torrent | head -1)
qa_cmd "appearance dark"

echo "== auto-start torrent, Choose Files… expanded in the popover"
add "$BUNNY"; qa_wait "starting state ~ items 1" 10
qa_cmd "starting show"; shot_popover "countdown"
qa_cmd "starting choose"; check "Choose Files… opens the list" "$(qa_wait "starting state ~ picked" 10)" "picked"
shot_popover "choose-expanded"
qa_cmd "starting cancel"; s=$(qa_wait "starting state ~ items 0" 10); echo "after Cancel: $s"
check "Cancel removes it" "$s" "items 0"

echo "== multi-file torrent awaiting a choice: compact first, the chevron opens it in place"
qa_setting transfers.ask_before_adding=multi_file
add "$SEASON"; qa_wait "starting state ~ items 1" 10
shot_popover "compact"
qa_cmd "starting fold"; state      # the chevron in front of the title
shot_popover "select-expanded-closed"
qa_cmd "starting open"; shot_popover "select-expanded"

echo "== two torrents in one popover: one expanded, the other compact"
qa_setting transfers.ask_before_adding=never
add "$BUNNY"; check "two torrents wait" "$(qa_wait "starting state ~ items 2" 10)" "items 2"
shot_popover "two-compact"
qa_cmd "starting choose Bunny"; state   # the chevron on the other: this one closes
shot_popover "two-switched"
qa_cmd "starting cancel Bunny"; qa_wait "starting state ~ items 1" 10 >/dev/null
qa_cmd "starting fold Night"; state

echo "== a selection made in the popover shows in the main window"
qa_cmd "starting none" "starting toggle S01E02" "starting toggle S01E03"
check "two episodes picked" "$(state)" "picked"
shot_popover "selection"
qa_cmd "window open" "section downloads" "chooser 0"
check "the chooser opens" "$(qa_wait "windows ~ chooser" 5)" "chooser"
shot_main "chooser-main"
qa_shot "addpop-chooser-window" chooser
qa_cmd "chooser-close"
qa_cmd "starting ok Night.Harbor"; s=$(qa_wait "starting state ~ items 0" 10); echo "after OK: $s"
check "OK starts it" "$s" "items 0"
shot_main "row-after-ok"
qa_end addpop
