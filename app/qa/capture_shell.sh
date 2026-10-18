#!/usr/bin/env bash
# The window shell and the menu bar menu (run after capture_files.sh, which builds the scene in
# seedwell-qa-files): the menu at rest and with hover (Open, Quit), the title area in Downloads and
# Sharing, rows with video icons, Settings as a tab, and the Play button on the largest video.
# Shots `shell-*.png`, dark and light, into app/build/qa/.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-files"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
shot_main() { qa_cmd "activate main" >/dev/null; local id; id=$("$OUT/winid" Seedwell | awk '$2==0 {print $1}' | head -1); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/shell-$1.png" && echo "  shell-$1"; }
shot_menu() { local id; for i in 1 2 3 4 5 6 7 8; do id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | head -1); [ -n "$id" ] && break; sleep 0.4; done
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/shell-$1.png" && echo "  shell-$1" || echo "  MISSING shell-$1"; }
for mode in ${MODES:-dark light}; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "tracker none" "search " "info none" "files collapse-all" "section downloads"
  shot_main "downloads-$mode"
  qa_cmd "section sharing"; sleep 1; shot_main "sharing-$mode"
  qa_cmd "settings general"; sleep 1; shot_main "settings-general-$mode"
  qa_cmd "settings folders"; sleep 1; shot_main "settings-folders-$mode"
  qa_cmd "section downloads"
  qa_cmd "menu open"; sleep 1; shot_menu "menu-rest-$mode"
  qa_cmd "menu-hover open"; sleep 0.5; shot_menu "menu-hover-open-$mode"
  qa_cmd "menu-hover quit"; sleep 0.5; shot_menu "menu-hover-quit-$mode"
  qa_cmd "menu-hover none" "menu close"
done
rm -f "$CTL.bench"
qa_cmd "play-row Buck" "play-row Night" "play-row Archive"
sleep 1; echo "--- play (largest video, launch blocked):"; cat "$CTL.bench"
